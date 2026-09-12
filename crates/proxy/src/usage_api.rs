//! Reading the provider's usage reporting with the Admin key.
//!
//! Two uses, both read-only. Recovering a day's consumption when the ledger
//! cannot be trusted, and checking that the traffic this organization sends is
//! still being served from the complimentary grant.
//!
//! The Admin key lives here and nowhere else: this client can only read usage,
//! so no inference path can reach it. Like the inference client, it never
//! resends, never follows redirects, and ignores system proxies.

use std::collections::HashMap;

use quotamiser_admission::epoch::SECONDS_PER_DAY;
use reqwest::{Client, Url, redirect};
use serde_json::Value;

/// The service tier the Usage API reports for traffic served from the
/// complimentary (data-sharing) grant. Measured 2026-09-11.
pub const INCENTIVIZED_TIER: &str = "incentivized-tier";

#[derive(Debug, thiserror::Error)]
pub enum UsageApiError {
    #[error("the usage base URL is invalid: {0}")]
    InvalidBaseUrl(String),
    #[error("the usage base URL must use https unless it is loopback: {0}")]
    InsecureBaseUrl(String),
    #[error("could not build the usage client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("the usage request failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("the usage API answered with status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the usage API answered with a body this code cannot read")]
    Unreadable,
}

/// What a day's usage reporting says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayUsage {
    /// Input plus output tokens, by model id.
    pub tokens_by_model: HashMap<String, u64>,
    /// Requests reported under a service tier that is not the complimentary
    /// one. Anything above zero means traffic was served outside the grant.
    pub requests_outside_the_grant: u64,
    /// Requests whose tier the reporting did not name. Not evidence either
    /// way; counted so it can be logged rather than silently read as proof.
    pub requests_of_unknown_tier: u64,
}

pub struct UsageApi {
    client: Client,
    base_url: String,
    admin_key: String,
}

impl std::fmt::Debug for UsageApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageApi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl UsageApi {
    /// `base_url` is the origin with its version prefix, the same one the
    /// inference client uses.
    pub fn new(base_url: &str, admin_key: String) -> Result<Self, UsageApiError> {
        let base = base_url.trim_end_matches('/').to_string();
        let url = Url::parse(&base).map_err(|_| UsageApiError::InvalidBaseUrl(base.clone()))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(UsageApiError::InsecureBaseUrl(base));
        }
        let client = Client::builder()
            .retry(reqwest::retry::never())
            .redirect(redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(UsageApiError::Build)?;
        Ok(Self {
            client,
            base_url: base,
            admin_key,
        })
    }

    /// Completions usage for one epoch (a UTC day index), by model and tier.
    pub async fn day(&self, epoch: i64) -> Result<DayUsage, UsageApiError> {
        let start = epoch.saturating_mul(SECONDS_PER_DAY);
        let end = start.saturating_add(SECONDS_PER_DAY);
        let mut usage = DayUsage::default();
        let mut page: Option<String> = None;
        // Bounded: the reporting returns one bucket a day, so a handful of
        // pages covers every grouping of it.
        for _ in 0..20 {
            let mut url = format!(
                "{}/organization/usage/completions\
                 ?start_time={start}&end_time={end}&bucket_width=1d\
                 &group_by=model&group_by=service_tier&limit=31",
                self.base_url
            );
            if let Some(cursor) = &page {
                url.push_str("&page=");
                url.push_str(cursor);
            }
            let response = self
                .client
                .get(&url)
                .bearer_auth(&self.admin_key)
                .send()
                .await
                .map_err(UsageApiError::Transport)?;
            let status = response.status();
            let body = response.text().await.map_err(UsageApiError::Transport)?;
            if !status.is_success() {
                return Err(UsageApiError::Status {
                    status: status.as_u16(),
                    body: body.chars().take(500).collect(),
                });
            }
            let parsed: Value =
                serde_json::from_str(&body).map_err(|_| UsageApiError::Unreadable)?;
            let buckets = parsed
                .get("data")
                .and_then(Value::as_array)
                .ok_or(UsageApiError::Unreadable)?;
            for bucket in buckets {
                for result in bucket
                    .get("results")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    absorb(&mut usage, result);
                }
            }
            match parsed.get("next_page").and_then(Value::as_str) {
                Some(cursor) if parsed["has_more"].as_bool() == Some(true) => {
                    page = Some(cursor.to_owned());
                }
                _ => break,
            }
        }
        Ok(usage)
    }
}

fn absorb(usage: &mut DayUsage, result: &Value) {
    let tokens = |name: &str| result.get(name).and_then(Value::as_u64).unwrap_or(0);
    let total = tokens("input_tokens").saturating_add(tokens("output_tokens"));
    if let Some(model) = result.get("model").and_then(Value::as_str) {
        *usage.tokens_by_model.entry(model.to_owned()).or_default() += total;
    }
    let requests = tokens("num_model_requests");
    match result.get("service_tier").and_then(Value::as_str) {
        Some(INCENTIVIZED_TIER) => {}
        Some(_) => usage.requests_outside_the_grant += requests,
        None => usage.requests_of_unknown_tier += requests,
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;
    use serde_json::json;

    use super::*;

    async fn serve(body: Value) -> String {
        let app = Router::new()
            .route(
                "/v1/organization/usage/completions",
                get(|State(body): State<Value>| async move { axum::Json(body) }),
            )
            .with_state(body);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/v1")
    }

    fn bucket(results: Value) -> Value {
        json!({"data": [{"start_time": 1, "end_time": 2, "results": results}], "has_more": false, "next_page": null})
    }

    #[tokio::test]
    async fn a_day_is_summed_by_model() {
        let base = serve(bucket(json!([
            {"model": "gpt-5.6-terra", "service_tier": "incentivized-tier", "num_model_requests": 9, "input_tokens": 2158, "output_tokens": 100},
            {"model": "gpt-5.6-sol", "service_tier": "incentivized-tier", "num_model_requests": 1, "input_tokens": 10, "output_tokens": 5}
        ])))
        .await;
        let usage = UsageApi::new(&base, "sk-admin".into())
            .unwrap()
            .day(1)
            .await
            .unwrap();
        assert_eq!(usage.tokens_by_model["gpt-5.6-terra"], 2258);
        assert_eq!(usage.tokens_by_model["gpt-5.6-sol"], 15);
        assert_eq!(usage.requests_outside_the_grant, 0);
        assert_eq!(usage.requests_of_unknown_tier, 0);
    }

    #[tokio::test]
    async fn traffic_outside_the_grant_is_counted_apart_from_unknown_tiers() {
        let base = serve(bucket(json!([
            {"model": "gpt-5.6-terra", "service_tier": "default", "num_model_requests": 3, "input_tokens": 100, "output_tokens": 10},
            {"model": "gpt-5.6-terra", "service_tier": null, "num_model_requests": 2, "input_tokens": 5, "output_tokens": 1}
        ])))
        .await;
        let usage = UsageApi::new(&base, "sk-admin".into())
            .unwrap()
            .day(1)
            .await
            .unwrap();
        assert_eq!(usage.requests_outside_the_grant, 3);
        assert_eq!(usage.requests_of_unknown_tier, 2);
        assert_eq!(usage.tokens_by_model["gpt-5.6-terra"], 116);
    }

    #[tokio::test]
    async fn a_failed_answer_is_an_error_not_an_empty_day() {
        let app = Router::new().route(
            "/v1/organization/usage/completions",
            get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = UsageApi::new(&format!("http://127.0.0.1:{port}/v1"), "sk-admin".into())
            .unwrap()
            .day(1)
            .await
            .expect_err("an error");
        assert!(
            matches!(error, UsageApiError::Status { status: 401, .. }),
            "{error}"
        );
    }

    #[test]
    fn a_plain_http_base_url_is_refused_unless_it_is_loopback() {
        assert!(UsageApi::new("http://example.com/v1", "k".into()).is_err());
        assert!(UsageApi::new("http://127.0.0.1:1/v1", "k".into()).is_ok());
        assert!(UsageApi::new("https://api.openai.com/v1", "k".into()).is_ok());
    }
}
