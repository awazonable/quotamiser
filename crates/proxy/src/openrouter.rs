//! The OpenRouter route: a second free provider, counted in requests.
//!
//! Nothing here reserves tokens. OpenRouter's free allowance is a number of
//! requests a day, and a request that fails spends one just the same, so the
//! slot is taken from the ledger's request counter before sending and never
//! given back.
//!
//! What may be sent is decided before sending, never by trying. Measured on
//! 2026-09-12, OpenRouter's Responses endpoint takes streaming, reasoning,
//! function tools, and namespaces of function tools — but the providers behind
//! it refuse `custom` tools, which is what Codex declares `apply_patch` as. A
//! request whose shape no configured model is known to accept is not sent:
//! finding out by being refused would cost one of the fifty.
//!
//! The keys live here. The inference key can only create responses and read
//! its own key's status; the management key, when configured, is used only to
//! list BYOK endpoints.

use std::future::Future;

use bytes::Bytes;
use quotamiser_protocol::request::ToolUsage;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, Response, Url, redirect};
use serde_json::Value;

use crate::config::{OpenRouterModel, ResolvedOpenRouter};

#[derive(Debug, thiserror::Error)]
pub enum OpenRouterError {
    #[error("the OpenRouter base URL is invalid: {0}")]
    InvalidBaseUrl(String),
    #[error("the OpenRouter base URL must use https unless it is loopback: {0}")]
    InsecureBaseUrl(String),
    #[error("could not build the OpenRouter client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("the OpenRouter request failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("OpenRouter answered with status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("OpenRouter answered with a body this code cannot read")]
    Unreadable,
}

/// What the account looks like right now. The route opens only on the shape
/// that makes paid usage impossible.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountCheck {
    pub free_tier: bool,
    pub total_credits: f64,
    pub total_usage: f64,
    /// `None` when no management key is configured, so BYOK was not checked.
    pub byok_endpoints: Option<usize>,
}

impl AccountCheck {
    /// Whether the zero-balance barrier holds: nothing bought, nothing owed,
    /// and no key of the operator's own behind which paid usage could run.
    ///
    /// Auto top-up has no endpoint that reports it (measured 2026-09-12), so
    /// it is not checked directly. A top-up that fired would show up here as
    /// credits above zero at the next check.
    pub fn safe(&self) -> bool {
        self.free_tier && self.total_credits <= 0.0 && self.byok_endpoints.is_none_or(|n| n == 0)
    }

    pub fn why_unsafe(&self) -> Option<String> {
        if self.safe() {
            return None;
        }
        let mut reasons = Vec::new();
        if !self.free_tier {
            reasons.push("the key is no longer on the free tier".to_string());
        }
        if self.total_credits > 0.0 {
            reasons.push(format!(
                "the account holds {} in credits",
                self.total_credits
            ));
        }
        if self.byok_endpoints.is_some_and(|n| n > 0) {
            reasons.push("the account has BYOK endpoints configured".to_string());
        }
        Some(reasons.join("; "))
    }
}

pub struct OpenRouter {
    client: Client,
    base_url: String,
    api_key: String,
    management_key: Option<String>,
}

impl std::fmt::Debug for OpenRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouter")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenRouter {
    pub fn new(config: &ResolvedOpenRouter) -> Result<Self, OpenRouterError> {
        let base = config.base_url.trim_end_matches('/').to_string();
        let url = Url::parse(&base).map_err(|_| OpenRouterError::InvalidBaseUrl(base.clone()))?;
        let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(OpenRouterError::InsecureBaseUrl(base));
        }
        let client = Client::builder()
            .retry(reqwest::retry::never())
            .redirect(redirect::Policy::none())
            .no_proxy()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .map_err(OpenRouterError::Build)?;
        Ok(Self {
            client,
            base_url: base,
            api_key: config.api_key.clone(),
            management_key: config.management_key.clone(),
        })
    }

    /// Starts generation. Crate-private, and only reached with a request slot
    /// already spent.
    pub(crate) fn create_response(
        &self,
        canonical_body: Bytes,
    ) -> impl Future<Output = reqwest::Result<Response>> {
        self.client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .header(CONTENT_TYPE, "application/json")
            .body(canonical_body)
            .send()
    }

    async fn get(&self, path: &str, key: &str) -> Result<Value, OpenRouterError> {
        let response = self
            .client
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(key)
            .send()
            .await
            .map_err(OpenRouterError::Transport)?;
        let status = response.status();
        let body = response.text().await.map_err(OpenRouterError::Transport)?;
        if !status.is_success() {
            return Err(OpenRouterError::Status {
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        serde_json::from_str(&body).map_err(|_| OpenRouterError::Unreadable)
    }

    /// Reads the account's shape. Consumes no part of the daily allowance.
    pub async fn verify_account(&self) -> Result<AccountCheck, OpenRouterError> {
        let key = self.get("/key", &self.api_key).await?;
        let data = key.get("data").ok_or(OpenRouterError::Unreadable)?;
        let free_tier = data
            .get("is_free_tier")
            .and_then(Value::as_bool)
            .ok_or(OpenRouterError::Unreadable)?;

        let credits = self.get("/credits", &self.api_key).await?;
        let credits = credits.get("data").ok_or(OpenRouterError::Unreadable)?;
        let number = |name: &str| credits.get(name).and_then(Value::as_f64).unwrap_or(0.0);

        let byok_endpoints = match &self.management_key {
            Some(management_key) => {
                let byok = self.get("/byok", management_key).await?;
                Some(
                    byok.get("data")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                )
            }
            None => None,
        };

        Ok(AccountCheck {
            free_tier,
            total_credits: number("total_credits"),
            total_usage: number("total_usage"),
            byok_endpoints,
        })
    }
}

/// The first configured model whose measured capabilities cover this request,
/// or `None` when the route cannot serve it at all.
pub fn model_for(
    models: &[OpenRouterModel],
    usage: ToolUsage,
    needs_structured_output: bool,
) -> Option<&OpenRouterModel> {
    models.iter().find(|model| {
        (!usage.function || model.function_tools)
            && (!usage.custom || model.custom_tools)
            && (!usage.namespace || model.namespace_tools)
            && (!usage.additional_tools || model.namespace_tools)
            && (!needs_structured_output || model.structured_outputs)
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::routing::get;
    use quotamiser_ledger::RequestWindow;
    use serde_json::json;

    use super::*;

    fn model(id: &str, custom: bool, namespace: bool) -> OpenRouterModel {
        OpenRouterModel {
            id: id.to_string(),
            function_tools: true,
            custom_tools: custom,
            namespace_tools: namespace,
            structured_outputs: true,
        }
    }

    fn config(base_url: String, management: Option<String>) -> ResolvedOpenRouter {
        ResolvedOpenRouter {
            base_url,
            api_key: "sk-or-test".into(),
            management_key: management,
            pool_id: "openrouter:free".into(),
            daily_request_limit: 50,
            window: RequestWindow {
                max_in_window: 20,
                window_seconds: 60,
            },
            account_ttl: 900,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
            models: vec![model("a/b:free", false, true)],
        }
    }

    async fn mock_account(free_tier: bool, credits: f64, byok: Value) -> String {
        let state = (free_tier, credits, byok);
        let app = Router::new()
            .route(
                "/api/v1/key",
                get(
                    |State((free, _, _)): State<(bool, f64, Value)>| async move {
                        axum::Json(json!({"data": {"is_free_tier": free, "usage": 0}}))
                    },
                ),
            )
            .route(
                "/api/v1/credits",
                get(
                    |State((_, credits, _)): State<(bool, f64, Value)>| async move {
                        axum::Json(json!({"data": {"total_credits": credits, "total_usage": 0}}))
                    },
                ),
            )
            .route(
                "/api/v1/byok",
                get(
                    |State((_, _, byok)): State<(bool, f64, Value)>| async move {
                        axum::Json(json!({ "data": byok }))
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://127.0.0.1:{port}/api/v1")
    }

    #[tokio::test]
    async fn a_zero_balance_free_tier_account_is_safe() {
        let base = mock_account(true, 0.0, json!([])).await;
        let router = OpenRouter::new(&config(base, None)).unwrap();
        let check = router.verify_account().await.unwrap();
        assert_eq!(
            check,
            AccountCheck {
                free_tier: true,
                total_credits: 0.0,
                total_usage: 0.0,
                byok_endpoints: None
            }
        );
        assert!(check.safe());
        assert_eq!(check.why_unsafe(), None);
    }

    #[tokio::test]
    async fn credits_on_the_account_close_the_route() {
        let base = mock_account(true, 10.0, json!([])).await;
        let router = OpenRouter::new(&config(base, None)).unwrap();
        let check = router.verify_account().await.unwrap();
        assert!(!check.safe());
        assert!(check.why_unsafe().unwrap().contains("credits"));
    }

    #[tokio::test]
    async fn leaving_the_free_tier_closes_the_route() {
        let base = mock_account(false, 0.0, json!([])).await;
        let router = OpenRouter::new(&config(base, None)).unwrap();
        assert!(!router.verify_account().await.unwrap().safe());
    }

    #[tokio::test]
    async fn byok_is_checked_only_when_a_management_key_is_configured() {
        let base = mock_account(true, 0.0, json!([{"provider": "openai"}])).await;

        let without = OpenRouter::new(&config(base.clone(), None)).unwrap();
        let check = without.verify_account().await.unwrap();
        assert_eq!(
            check.byok_endpoints, None,
            "not checked, not assumed absent"
        );
        assert!(check.safe());

        let with = OpenRouter::new(&config(base, Some("sk-or-mgmt".into()))).unwrap();
        let check = with.verify_account().await.unwrap();
        assert_eq!(check.byok_endpoints, Some(1));
        assert!(!check.safe());
        assert!(check.why_unsafe().unwrap().contains("BYOK"));
    }

    #[test]
    fn a_request_carrying_custom_tools_has_no_model_to_go_to() {
        let models = vec![
            model("a/b:free", false, true),
            model("openrouter/free", false, true),
        ];
        let custom = ToolUsage {
            function: true,
            custom: true,
            namespace: true,
            additional_tools: true,
        };
        assert!(
            model_for(&models, custom, false).is_none(),
            "measured: the providers behind OpenRouter refuse custom tools"
        );
    }

    #[test]
    fn the_first_model_that_covers_the_shape_is_chosen() {
        let models = vec![
            model("narrow/one:free", false, false),
            model("wider/two:free", false, true),
        ];
        let plain = ToolUsage {
            function: true,
            ..ToolUsage::default()
        };
        assert_eq!(
            model_for(&models, plain, false).unwrap().id,
            "narrow/one:free"
        );

        let with_namespace = ToolUsage {
            function: true,
            namespace: true,
            ..ToolUsage::default()
        };
        assert_eq!(
            model_for(&models, with_namespace, false).unwrap().id,
            "wider/two:free"
        );

        let lite = ToolUsage {
            function: true,
            namespace: true,
            additional_tools: true,
            ..ToolUsage::default()
        };
        assert_eq!(
            model_for(&models, lite, false).unwrap().id,
            "wider/two:free"
        );
    }

    #[test]
    fn a_structured_output_needs_a_model_that_does_them() {
        let mut narrow = model("narrow/one:free", false, true);
        narrow.structured_outputs = false;
        let models = vec![narrow, model("wider/two:free", false, true)];
        assert_eq!(
            model_for(&models, ToolUsage::default(), true).unwrap().id,
            "wider/two:free"
        );
    }

    #[test]
    fn a_plain_http_base_url_is_refused_unless_it_is_loopback() {
        assert!(OpenRouter::new(&config("http://openrouter.ai/api/v1".into(), None)).is_err());
        assert!(OpenRouter::new(&config("http://127.0.0.1:1/api/v1".into(), None)).is_ok());
        assert!(OpenRouter::new(&config("https://openrouter.ai/api/v1".into(), None)).is_ok());
    }
}
