//! The heartbeat receiver: a single `POST /beat/{job}` route.
//!
//! The wire protocol is the interface. Monitored jobs are written in Python,
//! TypeScript, Go, Bash and Rust, so the only thing required to integrate is an
//! HTTP POST:
//!
//! ```text
//! curl -fsS -X POST localhost:9111/beat/nightly-sync
//! ```
//!
//! Every field of the body is optional and a bare POST with no body at all is a
//! valid heartbeat.

use std::collections::BTreeMap;
use std::time::SystemTime;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;

use crate::state::SharedState;

/// The optional JSON body of a heartbeat.
///
/// `worked`, `data_ts` and `counters` are accepted and logged but not yet acted
/// on; liveness is the only signal this version evaluates. They are part of the
/// documented protocol, so rejecting them would break jobs that already send
/// them.
#[derive(Debug, Deserialize)]
pub struct Beat {
    /// Whether the job actually did work this time round, as opposed to merely
    /// running.
    pub worked: Option<bool>,

    /// Unix seconds: how fresh the data the job acted on was.
    pub data_ts: Option<i64>,

    /// Named counters. The job decides what they mean.
    pub counters: Option<BTreeMap<String, f64>>,
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/beat/{job}", post(beat))
        .with_state(state)
}

async fn beat(
    State(state): State<SharedState>,
    Path(job): Path<String>,
    body: Option<Json<Beat>>,
) -> Response {
    if let Some(Json(beat)) = &body {
        tracing::debug!(
            %job,
            worked = ?beat.worked,
            data_ts = ?beat.data_ts,
            counters = ?beat.counters,
            "beat carried a body"
        );
    }

    if !state.record_beat(&job, SystemTime::now()) {
        // Accepting a beat for a name nobody configured would leave the real
        // job unwatched and say nothing about it — the exact quiet failure this
        // tool exists to catch. Say so, loudly, to both ends.
        tracing::warn!(%job, "beat for a job that is not in the config; ignoring it");
        return (
            StatusCode::NOT_FOUND,
            format!("no job named {job:?} is configured\n"),
        )
            .into_response();
    }

    tracing::trace!(%job, "beat");
    (StatusCode::OK, "ok\n").into_response()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::config::{AliveConfig, JobConfig};
    use crate::state::State as JobStates;

    fn shared() -> SharedState {
        let jobs = vec![
            JobConfig {
                name: "product-scraper".into(),
                alive: Some(AliveConfig {
                    expect_every: Duration::from_secs(60),
                    warn_after: Duration::from_secs(300),
                    critical_after: Duration::from_secs(900),
                }),
            },
            JobConfig {
                name: "nightly-sync".into(),
                alive: None,
            },
        ];
        SharedState::new(JobStates::new(SystemTime::now(), &jobs))
    }

    async fn post_beat(state: &SharedState, job: &str, body: Option<&str>) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/beat/{job}"));
        let request = match body {
            Some(json) => request
                .header("content-type", "application/json")
                .body(Body::from(json.to_string())),
            None => request.body(Body::empty()),
        }
        .expect("request should build");

        let response = router(state.clone())
            .oneshot(request)
            .await
            .expect("router should respond");

        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body should collect")
            .to_bytes();

        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    fn beats(state: &SharedState, job: &str) -> u64 {
        state.read(|s| s.job(job).map(|j| j.beats).unwrap_or_default())
    }

    #[tokio::test]
    async fn a_bare_post_is_a_valid_heartbeat() {
        let state = shared();

        let (status, _) = post_beat(&state, "nightly-sync", None).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(beats(&state, "nightly-sync"), 1);
    }

    #[tokio::test]
    async fn a_post_with_the_full_documented_body_is_accepted() {
        let state = shared();

        let (status, _) = post_beat(
            &state,
            "product-scraper",
            Some(r#"{"worked":true,"data_ts":1724500000,"counters":{"fetched":120,"parsed":118}}"#),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(beats(&state, "product-scraper"), 1);
    }

    /// The same route has to serve a scraper reporting fetch counters and an ETL
    /// reporting row counts, without knowing what either means.
    #[tokio::test]
    async fn counter_names_are_not_interpreted() {
        let state = shared();

        let (scraper, _) = post_beat(
            &state,
            "product-scraper",
            Some(r#"{"counters":{"fetched":120,"parsed":118}}"#),
        )
        .await;
        let (etl, _) = post_beat(
            &state,
            "nightly-sync",
            Some(r#"{"worked":true,"counters":{"rows_read":8400,"rows_written":8400}}"#),
        )
        .await;

        assert_eq!(scraper, StatusCode::OK);
        assert_eq!(etl, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_empty_json_object_is_accepted() {
        let state = shared();

        let (status, _) = post_beat(&state, "product-scraper", Some("{}")).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(beats(&state, "product-scraper"), 1);
    }

    #[tokio::test]
    async fn a_beat_for_an_unknown_job_is_a_404_naming_the_job() {
        let state = shared();

        let (status, body) = post_beat(&state, "product-scrapper", None).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("product-scrapper"), "unhelpful body: {body}");
    }

    #[tokio::test]
    async fn malformed_json_is_rejected_rather_than_silently_counted() {
        let state = shared();

        let (status, _) = post_beat(&state, "product-scraper", Some("{not json")).await;

        assert!(status.is_client_error(), "expected a 4xx, got {status}");
        assert_eq!(
            beats(&state, "product-scraper"),
            0,
            "a body we could not parse must not count as a heartbeat"
        );
    }

    #[tokio::test]
    async fn the_route_only_answers_post() {
        let state = shared();
        let request = Request::builder()
            .method("GET")
            .uri("/beat/nightly-sync")
            .body(Body::empty())
            .expect("request should build");

        let response = router(state)
            .oneshot(request)
            .await
            .expect("router should respond");

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn beats_accumulate_per_job() {
        let state = shared();

        post_beat(&state, "product-scraper", None).await;
        post_beat(&state, "product-scraper", None).await;
        post_beat(&state, "nightly-sync", None).await;

        assert_eq!(beats(&state, "product-scraper"), 2);
        assert_eq!(beats(&state, "nightly-sync"), 1);
    }
}
