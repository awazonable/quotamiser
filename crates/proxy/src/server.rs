//! The client-facing HTTP surface: one create endpoint, and a model listing.
//!
//! Every refusal has a shape chosen for how clients react to it. A request the
//! allowlist rejects is a 400, which Codex shows and stops on. "Nothing can
//! serve you" is a 429 carrying `usage_limit_reached`, which Codex does not
//! retry. A WebSocket upgrade is 426, which makes Codex fall back to HTTP
//! immediately rather than treating the endpoint as broken.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use quotamiser_admission::epoch::{end_of, epoch_of};
use quotamiser_ledger::Refusal;
use quotamiser_protocol::error_body::{openai_error_body, usage_limit_reached_body};
use quotamiser_protocol::request::CreateRequest;
use serde_json::json;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use crate::admission::{Decision, Refused};
use crate::dispatch::Head;
use crate::runtime::Runtime;

/// Codex sends its whole conversation every turn, so the cap is generous;
/// it exists to stop an unbounded body, not to shape usage.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

const PROVIDER_HEADER: &str = "x-quotamiser-provider";
const MODEL_HEADER: &str = "x-quotamiser-model";

pub fn router(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route("/v1/responses", post(create_response))
        .route("/v1/models", get(list_models))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(runtime)
}

async fn list_models(State(runtime): State<Arc<Runtime>>) -> Response {
    let data: Vec<_> = runtime
        .catalog()
        .keys()
        .map(|id| json!({"id": id, "object": "model", "owned_by": "openai"}))
        .collect();
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

async fn create_response(
    State(runtime): State<Arc<Runtime>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if headers.contains_key(header::UPGRADE) {
        return error(
            StatusCode::UPGRADE_REQUIRED,
            "QuotaMiser serves this endpoint over HTTP only.",
        );
    }

    let request = match CreateRequest::parse(&body) {
        Ok(request) => request,
        Err(rejection) => {
            return (StatusCode::BAD_REQUEST, axum::Json(rejection.error_body())).into_response();
        }
    };

    let Some(now) = runtime.trusted_now() else {
        return unavailable_until(
            "QuotaMiser has no trusted reading of the provider's clock, so it cannot tell which day's quota this would spend.",
            None,
        );
    };

    let mut window = match runtime.window().await {
        Ok(window) => window,
        Err(error) => return error_detail(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    // A request arriving between two runs of the clock loop opens the day
    // itself rather than waiting for the loop.
    if let quotamiser_admission::epoch::Window::RolloverDue { epoch } = window {
        if let Err(error) = runtime.ensure_epoch(epoch).await {
            return error_detail(StatusCode::SERVICE_UNAVAILABLE, &error.to_string());
        }
        window = match runtime.window().await {
            Ok(window) => window,
            Err(error) => return error_detail(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
        };
    }

    let model = request.model().to_owned();
    let permit = match runtime.admitter().admit(&request, now, window).await {
        Decision::Admitted(permit) => permit,
        Decision::Refused(refused) => return refusal_response(refused, now),
    };

    let dispatched = runtime.dispatcher().dispatch(permit).await;
    match dispatched.head {
        Head::Stream {
            status,
            content_type,
            body,
        } => {
            let stream = ReceiverStream::new(body).map(Ok::<Bytes, Infallible>);
            let mut response = Response::builder()
                .status(status)
                .body(Body::from_stream(stream))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                content_type.unwrap_or_else(|| HeaderValue::from_static("text/event-stream")),
            );
            attribute(headers, &model);
            response
        }
        // Upstream refused the request itself; the client sees its answer.
        Head::Refused { status, body, .. } => {
            let mut response = (status, body).into_response();
            attribute(response.headers_mut(), &model);
            response
        }
        Head::RateLimited { retry_after } | Head::CoolingDown { retry_after } => {
            let seconds = retry_after.as_secs().max(1);
            let mut response = unavailable_until(
                "The provider is not accepting requests right now. Nothing was sent, and no quota was spent.",
                Some(now + seconds as i64),
            );
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
        Head::NotSent(reason) | Head::OutcomeUnknown(reason) => {
            error_detail(StatusCode::SERVICE_UNAVAILABLE, &reason)
        }
    }
}

fn refusal_response(refused: Refused, now: i64) -> Response {
    let end_of_day = end_of(epoch_of(now));
    match refused {
        Refused::UnknownModel(model) => (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": {
                    "message": format!("`{model}` is not a model QuotaMiser is configured to serve."),
                    "type": "invalid_request_error",
                    "param": "model",
                    "code": "model_not_configured",
                }
            })),
        )
            .into_response(),
        Refused::Ledger(Refusal::InsufficientQuota {
            remaining,
            liability,
        }) => unavailable_until(
            &format!(
                "This request could consume up to {liability} tokens and only {remaining} remain in today's free grant. Nothing was sent."
            ),
            Some(end_of_day),
        ),
        Refused::Ledger(Refusal::PoolNotOpen { pool_id, epoch }) => unavailable_until(
            &format!("Pool {pool_id} is not open for epoch {epoch}."),
            Some(end_of_day),
        ),
        Refused::WindowClosed(reason) => unavailable_until(
            &format!(
                "QuotaMiser is not spending quota right now ({reason:?}): the day boundary is too close to tell which day this would be charged to."
            ),
            Some(end_of_day),
        ),
        Refused::RolloverDue { epoch } => unavailable_until(
            &format!("The ledger is still opening epoch {epoch}."),
            None,
        ),
        Refused::Ledger(refusal) => unavailable_until(
            &format!("QuotaMiser is not admitting requests: {refusal:?}. This needs an operator, not a retry."),
            None,
        ),
        Refused::InputNotCounted(detail) => error_detail(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("The input could not be counted, so no upper bound on its cost exists: {detail}"),
        ),
        Refused::Estimate(error) => {
            error_detail(StatusCode::SERVICE_UNAVAILABLE, &error.to_string())
        }
        Refused::LedgerUnavailable(detail) => {
            error_detail(StatusCode::SERVICE_UNAVAILABLE, &detail)
        }
    }
}

fn attribute(headers: &mut HeaderMap, model: &str) {
    headers.insert(PROVIDER_HEADER, HeaderValue::from_static("openai"));
    if let Ok(value) = HeaderValue::from_str(model) {
        headers.insert(MODEL_HEADER, value);
    }
}

/// A 429 the client should not retry until `resets_at`.
fn unavailable_until(message: &str, resets_at: Option<i64>) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(usage_limit_reached_body(message, resets_at)),
    )
        .into_response()
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(openai_error_body(status.as_u16(), message)),
    )
        .into_response()
}

fn error_detail(status: StatusCode, detail: &str) -> Response {
    error(status, detail)
}
