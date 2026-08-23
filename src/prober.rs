//! Probing dependencies directly.
//!
//! One task per check, on its own interval. This module is the I/O half of
//! degradation detection: it makes requests, times them, and records what
//! happened. Deciding whether a latency is *bad* belongs to the evaluator, which
//! stays a pure function of the recorded observations and the clock it is
//! handed.

use std::time::{Duration, Instant, SystemTime};

use reqwest::Client;
use tokio::time::MissedTickBehavior;

use crate::config::{CheckConfig, ProbeConfig};
use crate::state::{Observation, Outcome, SharedState};

/// Runs one check forever, recording an observation per interval.
pub async fn run(check: CheckConfig, client: Client, state: SharedState) {
    let mut ticker = tokio::time::interval(check.interval);
    // A suspended machine must not produce a burst of back-to-back probes that
    // all land in the same instant and distort the latency window.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        let outcome = probe(&check, &client).await;
        let observation = Observation {
            at: SystemTime::now(),
            outcome,
        };

        match &observation.outcome {
            Outcome::Responded(latency) => {
                tracing::trace!(check = %check.name, ?latency, "probe ok")
            }
            Outcome::Failed(error) => {
                tracing::debug!(check = %check.name, %error, "probe failed")
            }
        }

        state.record_probe(&check.name, observation);
    }
}

/// Makes one request and reports how it went.
///
/// The timer covers the whole exchange including reading the body, because a
/// dependency that sends headers promptly and then stalls is exactly the kind of
/// degradation this is looking for.
async fn probe(check: &CheckConfig, client: &Client) -> Outcome {
    let started = Instant::now();

    let request = match &check.probe {
        ProbeConfig::Http { url } => client.get(url.clone()),
        ProbeConfig::JsonRpc { url, method } => client.post(url.clone()).json(&json_rpc(method)),
    };

    let response = match request.timeout(check.timeout).send().await {
        Ok(response) => response,
        Err(err) => return Outcome::Failed(describe(err, check.timeout)),
    };

    let status = response.status();
    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => return Outcome::Failed(describe(err, check.timeout)),
    };
    let elapsed = started.elapsed();

    if !status.is_success() {
        return Outcome::Failed(format!("responded {status}"));
    }

    // A JSON-RPC server answers 200 while telling you the call failed, which is
    // precisely the "healthy but wrong" shape this tool exists for.
    if let ProbeConfig::JsonRpc { .. } = &check.probe {
        if let Some(error) = json_rpc_error(&body) {
            return Outcome::Failed(format!("jsonrpc error: {error}"));
        }
    }

    Outcome::Responded(elapsed)
}

fn json_rpc(method: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": [],
    })
}

/// Extracts a JSON-RPC `error` member, if the response carries one.
fn json_rpc_error(body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = parsed.get("error")?;

    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("no message");

    Some(
        match error.get("code").and_then(serde_json::Value::as_i64) {
            Some(code) => format!("{message} (code {code})"),
            None => message.to_string(),
        },
    )
}

/// Turns a transport error into something worth putting in an alert.
///
/// `without_url` is not strictly needed here — check URLs are not secrets the
/// way a bot token is — but a query string can carry an API key, and the URL is
/// already named in the alert by the check's own name.
fn describe(err: reqwest::Error, timeout: Duration) -> String {
    let err = err.without_url();

    if err.is_timeout() {
        return format!("no response within {}", crate::fmt::duration(timeout));
    }
    if err.is_connect() {
        return format!("could not connect: {err}");
    }
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jsonrpc_request_carries_the_configured_method() {
        let body = json_rpc("eth_blockNumber");

        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["method"], "eth_blockNumber");
        assert!(body.get("params").is_some());
    }

    /// A JSON-RPC server answers 200 and puts the failure in the body. Treating
    /// that as a healthy 90ms response is the exact failure this tool is about.
    #[test]
    fn a_jsonrpc_error_body_is_recognised_despite_a_200() {
        let body =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;

        let error = json_rpc_error(body).expect("should be recognised as an error");
        assert!(error.contains("method not found"), "{error}");
        assert!(error.contains("-32601"), "{error}");
    }

    #[test]
    fn a_successful_jsonrpc_body_is_not_an_error() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":"0x10d4f"}"#;
        assert_eq!(json_rpc_error(body), None);
    }

    #[test]
    fn a_body_that_is_not_json_is_not_reported_as_a_jsonrpc_error() {
        assert_eq!(json_rpc_error("<html>gateway timeout</html>"), None);
    }

    #[test]
    fn a_jsonrpc_error_without_a_code_still_reads() {
        let body = r#"{"error":{"message":"upstream unavailable"}}"#;
        assert_eq!(
            json_rpc_error(body),
            Some("upstream unavailable".to_string())
        );
    }
}
