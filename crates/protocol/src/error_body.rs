//! OpenAI-shaped error bodies.
//!
//! Derived from tokenmiser's `openai_error_body`, `crates/tokenmiser-proxy/src/sse.rs`
//! at commit 5fe22e826a0fde09b6910b273dc45bed24316f9f. MIT License, Copyright
//! (c) 2026 Open Intelligence Labs contributors; see LICENSE.

use serde_json::{Value, json};

/// Maps the HTTP status onto OpenAI's error `type` vocabulary, so that client
/// SDKs handle the error the way they would handle OpenAI's own.
pub fn openai_error_body(status: u16, message: &str) -> Value {
    let error_type = match status {
        401 | 403 => "authentication_error",
        402 => "insufficient_quota",
        404 => "not_found_error",
        409 => "conflict_error",
        429 => "rate_limit_error",
        400..=499 => "invalid_request_error",
        _ => "api_error",
    };
    json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": null,
            "code": null,
        }
    })
}

/// A 400 for a request QuotaMiser will not send.
///
/// Returned as 400 rather than 403 or 422: Codex shows a 400's message and
/// stops, but retries a 403 or 422 as an unexpected status, re-sending a
/// request that can only be refused again.
pub fn invalid_request_body(message: &str, param: Option<&str>, code: &str) -> Value {
    json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": param,
            "code": code,
        }
    })
}

/// A 429 telling the client that nothing can serve it until `resets_at`
/// (Unix seconds), when that is known.
///
/// Codex reads `type: "usage_limit_reached"` as a usage limit, which it does
/// not retry, and shows the reset time. A plain 429 would be retried.
pub fn usage_limit_reached_body(message: &str, resets_at: Option<i64>) -> Value {
    json!({
        "error": {
            "message": message,
            "type": "usage_limit_reached",
            "param": null,
            "code": "usage_limit_reached",
            "resets_at": resets_at,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_names_the_parameter_and_code() {
        let body = invalid_request_body("no", Some("tools[0].type"), "hosted_tool_unsupported");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["param"], "tools[0].type");
        assert_eq!(body["error"]["code"], "hosted_tool_unsupported");
        assert_eq!(body["error"]["message"], "no");
    }

    #[test]
    fn a_usage_limit_carries_the_reset_time() {
        let body = usage_limit_reached_body("wait", Some(1_789_171_200));
        assert_eq!(body["error"]["type"], "usage_limit_reached");
        assert_eq!(body["error"]["resets_at"], 1_789_171_200);
        assert!(usage_limit_reached_body("wait", None)["error"]["resets_at"].is_null());
    }

    #[test]
    fn the_shape_matches_openai() {
        let body = openai_error_body(400, "bad");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "bad");
        assert!(body["error"].get("param").is_some());
        assert!(body["error"].get("code").is_some());
        assert_eq!(
            openai_error_body(402, "x")["error"]["type"],
            "insufficient_quota"
        );
        assert_eq!(openai_error_body(502, "x")["error"]["type"], "api_error");
    }
}
