//! Trusted time from the provider.
//!
//! The epoch window never trusts the local wall clock. It takes a reading of
//! the provider's clock — the `Date` header of a successful model listing,
//! which costs no quota and answers whether or not inference is open — and
//! advances it by elapsed time on a monotonic clock, which wall-clock
//! adjustments cannot move.

use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use quotamiser_admission::epoch::TrustedReading;
use reqwest::header::DATE;
use tokio::time::Instant;

use crate::upstream::{Upstream, UpstreamError};

#[derive(Debug, thiserror::Error)]
pub enum ClockError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// Only a successful answer is taken as the provider's clock. Anything
    /// else may not have come from the provider.
    #[error("the provider answered the time request with status {0}")]
    Status(u16),
    #[error("the provider's response carried no usable Date header")]
    NoDate,
}

pub struct TrustedClock {
    upstream: Arc<Upstream>,
    origin: Instant,
    last: Mutex<Option<TrustedReading>>,
}

impl TrustedClock {
    pub fn new(upstream: Arc<Upstream>) -> Self {
        Self {
            upstream,
            origin: Instant::now(),
            last: Mutex::new(None),
        }
    }

    /// Monotonic seconds since this clock was created. This, not wall time, is
    /// what the epoch window takes as its local clock.
    pub fn monotonic_now(&self) -> i64 {
        i64::try_from(self.origin.elapsed().as_secs()).unwrap_or(i64::MAX)
    }

    pub fn last_reading(&self) -> Option<TrustedReading> {
        *self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub async fn refresh(&self) -> Result<TrustedReading, ClockError> {
        let response = self.upstream.list_models().await?;
        let local_unix = self.monotonic_now();
        let status = response.status();
        if !status.is_success() {
            return Err(ClockError::Status(status.as_u16()));
        }
        let upstream_unix = response
            .headers()
            .get(DATE)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| httpdate::parse_http_date(text).ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_secs()).ok())
            .ok_or(ClockError::NoDate)?;
        let reading = TrustedReading {
            upstream_unix,
            local_unix,
        };
        *self
            .last
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reading);
        Ok(reading)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use quotamiser_admission::epoch::epoch_of;

    use super::*;
    use crate::upstream::UpstreamConfig;

    async fn clock_against(status: StatusCode, date: &'static str) -> TrustedClock {
        let router = Router::new().route(
            "/v1/models",
            get(move || async move { (status, [(DATE, date)], "{}").into_response() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let upstream = Upstream::new(UpstreamConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test-key".into(),
            project: None,
            connect_timeout: Duration::from_secs(2),
            read_timeout: Duration::from_secs(5),
        })
        .unwrap();
        TrustedClock::new(Arc::new(upstream))
    }

    #[tokio::test]
    async fn the_provider_date_header_is_the_reading() {
        let clock = clock_against(StatusCode::OK, "Fri, 11 Sep 2026 00:00:05 GMT").await;
        let reading = clock.refresh().await.unwrap();
        assert_eq!(reading.upstream_unix, 1_789_084_805);
        assert_eq!(epoch_of(reading.upstream_unix), 20_707);
        assert_eq!(clock.last_reading(), Some(reading));
    }

    #[tokio::test]
    async fn an_unsuccessful_answer_is_not_a_time_source() {
        let clock = clock_against(StatusCode::UNAUTHORIZED, "Fri, 11 Sep 2026 00:00:05 GMT").await;
        assert!(matches!(
            clock.refresh().await,
            Err(ClockError::Status(401))
        ));
        assert_eq!(clock.last_reading(), None);
    }

    #[tokio::test]
    async fn a_garbled_date_is_refused() {
        let clock = clock_against(StatusCode::OK, "yesterday").await;
        assert!(matches!(clock.refresh().await, Err(ClockError::NoDate)));
    }
}
