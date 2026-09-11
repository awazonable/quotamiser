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

#[cfg(test)]
mod tests {
    use super::*;

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
