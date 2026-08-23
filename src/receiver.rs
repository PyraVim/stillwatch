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
use std::time::{Duration, SystemTime};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use serde::Deserialize;

use crate::state::{BeatDetail, SharedState};

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

impl Beat {
    /// Converts the wire body into recorded state, refusing values that cannot
    /// mean anything.
    ///
    /// A `NaN` counter would silently poison every ratio it appears in — every
    /// comparison against it is false, so the rule would quietly stop firing
    /// forever. A negative count of things that happened is not a smaller
    /// number, it is a mistake. Both are refused at the door with a 400 rather
    /// than stored and reasoned about later.
    fn into_detail(self) -> Result<BeatDetail, String> {
        let counters = self.counters.unwrap_or_default();
        for (name, value) in &counters {
            if !value.is_finite() {
                return Err(format!(
                    "counter {name:?} is {value}, which is not a number"
                ));
            }
            if *value < 0.0 {
                return Err(format!("counter {name:?} is negative ({value})"));
            }
        }

        let data_ts = match self.data_ts {
            None => None,
            Some(seconds) if seconds < 0 => {
                return Err(format!(
                    "data_ts {seconds} is before 1970, which is not a time any data was read at"
                ))
            }
            Some(seconds) => Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds as u64)),
        };

        Ok(BeatDetail {
            worked: self.worked,
            data_ts,
            counters,
        })
    }
}

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/beat/{job}", post(beat))
        .with_state(state)
}

async fn beat(State(state): State<SharedState>, Path(job): Path<String>, body: Bytes) -> Response {
    // The body is parsed here rather than by a `Json` extractor on purpose.
    // `Json` refuses anything whose content-type is not JSON, and plenty of
    // HTTP clients attach a default content-type even to a request with no body
    // at all — which would turn a documented bare heartbeat into a 415 for some
    // languages and not others. The protocol says every field is optional, so
    // an empty body is simply an empty body, whatever the headers claim.
    let detail = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match serde_json::from_slice::<Beat>(&body) {
            Ok(beat) => Some(beat),
            Err(err) => {
                tracing::warn!(%job, %err, "beat body is not valid json; not counting it");
                return (
                    StatusCode::BAD_REQUEST,
                    format!("body is not valid json: {err}\n"),
                )
                    .into_response();
            }
        }
    };

    let detail = match detail {
        Some(beat) => {
            tracing::debug!(
                %job,
                worked = ?beat.worked,
                data_ts = ?beat.data_ts,
                counters = ?beat.counters,
                "beat carried a body"
            );
            match beat.into_detail() {
                Ok(detail) => detail,
                Err(complaint) => {
                    tracing::warn!(%job, %complaint, "rejecting a beat; not counting it");
                    return (StatusCode::BAD_REQUEST, format!("{complaint}\n")).into_response();
                }
            }
        }
        None => BeatDetail::default(),
    };

    if !state.record_beat_with(&job, SystemTime::now(), &detail) {
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
                alive: Some(AliveConfig {
                    expect_every: Duration::from_secs(60),
                    warn_after: Duration::from_secs(300),
                    critical_after: Duration::from_secs(900),
                }),
                ..JobConfig::named("product-scraper")
            },
            JobConfig {
                alive: None,
                ..JobConfig::named("nightly-sync")
            },
        ];
        SharedState::new(JobStates::new(SystemTime::now(), &jobs, &[]))
    }

    async fn post_beat(state: &SharedState, job: &str, body: Option<&str>) -> (StatusCode, String) {
        match body {
            Some(json) => post_raw(state, job, Some("application/json"), json).await,
            None => post_raw(state, job, None, "").await,
        }
    }

    async fn post_raw(
        state: &SharedState,
        job: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/beat/{job}"));
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let request = builder
            .body(Body::from(body.to_string()))
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

    /// Regression: an earlier version used axum's `Option<Json<Beat>>`, which
    /// answers 415 whenever the content-type is set to anything that is not
    /// JSON. Several HTTP clients attach a default content-type even to a
    /// bodyless POST, so the documented bare heartbeat worked from curl and
    /// failed from PowerShell. The body is what decides, never the header.
    #[tokio::test]
    async fn a_bare_post_is_accepted_whatever_content_type_the_client_invents() {
        for content_type in [
            None,
            Some("application/x-www-form-urlencoded"),
            Some("text/plain"),
            Some("application/json"),
        ] {
            let state = shared();

            let (status, _) = post_raw(&state, "nightly-sync", content_type, "").await;

            assert_eq!(
                status,
                StatusCode::OK,
                "a bare POST with content-type {content_type:?} must be a valid heartbeat"
            );
            assert_eq!(beats(&state, "nightly-sync"), 1);
        }
    }

    #[tokio::test]
    async fn a_json_body_is_read_even_without_a_json_content_type() {
        let state = shared();

        let (status, _) = post_raw(
            &state,
            "product-scraper",
            Some("text/plain"),
            r#"{"worked":true}"#,
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(beats(&state, "product-scraper"), 1);
    }

    #[tokio::test]
    async fn a_whitespace_only_body_counts_as_no_body() {
        let state = shared();

        let (status, _) = post_raw(&state, "nightly-sync", None, "\n  \n").await;

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

    // -- the detail a beat carries -----------------------------------------

    #[tokio::test]
    async fn a_beat_records_worked_data_ts_and_counters() {
        let state = shared();

        let (status, _) = post_beat(
            &state,
            "product-scraper",
            Some(r#"{"worked":true,"data_ts":1724500000,"counters":{"fetched":120,"parsed":118}}"#),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        state.read(|s| {
            let job = s.job("product-scraper").expect("job");
            assert!(job.last_worked.is_some());
            assert_eq!(
                job.last_data_ts,
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_500_000))
            );
            assert!(job.has_ever_seen("fetched"));
            assert!(job.has_ever_seen("parsed"));
            assert!(!job.has_ever_seen("stored"));
        });
    }

    /// A bare beat says the loop ran. It does not claim any work was done, and
    /// treating it as work would collapse the two signals the tool exists to
    /// keep apart.
    #[tokio::test]
    async fn a_bare_beat_does_not_count_as_work() {
        let state = shared();

        post_beat(&state, "product-scraper", None).await;
        post_beat(&state, "product-scraper", Some(r#"{"worked":false}"#)).await;

        state.read(|s| {
            let job = s.job("product-scraper").expect("job");
            assert_eq!(job.beats, 2);
            assert_eq!(
                job.last_worked, None,
                "only an explicit worked:true marks work"
            );
        });
    }

    /// `NaN` and infinity compare false against everything, so a single one
    /// would silently stop a ratio rule firing for good.
    ///
    /// JSON has no literal for either, and `serde_json` refuses out-of-range
    /// numbers before the guard in `into_detail` is reached — so what is
    /// asserted here is the property rather than which layer enforces it. The
    /// guard stays as a backstop because `BeatDetail` is public and the cost of
    /// being wrong is a rule that never fires again.
    #[tokio::test]
    async fn a_counter_that_is_not_a_finite_number_never_reaches_state() {
        let state = shared();

        for body in [
            r#"{"counters":{"fetched":1e999}}"#,
            r#"{"counters":{"fetched":-1e999}}"#,
        ] {
            let (status, _) = post_beat(&state, "product-scraper", Some(body)).await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "for {body}");
            assert_eq!(
                beats(&state, "product-scraper"),
                0,
                "a value that would poison every ratio must not be recorded"
            );
        }
    }

    #[test]
    fn the_backstop_refuses_non_finite_counters_directly() {
        let beat = Beat {
            worked: None,
            data_ts: None,
            counters: Some(BTreeMap::from([("fetched".to_string(), f64::NAN)])),
        };

        let err = beat.into_detail().expect_err("NaN must be refused");
        assert!(err.contains("fetched"), "{err}");
    }

    #[tokio::test]
    async fn a_negative_counter_is_refused() {
        let state = shared();

        let (status, body) = post_beat(
            &state,
            "product-scraper",
            Some(r#"{"counters":{"parsed":-4}}"#),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("negative"), "{body}");
        assert_eq!(beats(&state, "product-scraper"), 0);
    }

    #[tokio::test]
    async fn a_data_ts_before_the_epoch_is_refused() {
        let state = shared();

        let (status, body) = post_beat(&state, "product-scraper", Some(r#"{"data_ts":-5}"#)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("1970"), "{body}");
    }

    #[tokio::test]
    async fn a_zero_counter_is_perfectly_valid() {
        let state = shared();

        let (status, _) = post_beat(
            &state,
            "product-scraper",
            Some(r#"{"counters":{"fetched":10,"parsed":0}}"#),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "parsing nothing is a real outcome");
        assert_eq!(beats(&state, "product-scraper"), 1);
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
