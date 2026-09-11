//! What an upstream HTTP status means for the client, the provider, and the
//! reservation.
//!
//! Statuses are decided one by one, never as broad classes. Treating every
//! 4xx as the client's fault would return OpenRouter's 402 to the client, when
//! that 402 is exactly what the zero-balance safety design produces and the
//! request should move on to the next provider.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    /// Relay the upstream response stream.
    Relay,
    /// The request itself is at fault. Return the error; another provider
    /// would reject it too.
    ReturnError,
    /// Try the next provider in the fallback chain.
    TryNextProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAction {
    Keep,
    /// A configuration or safety failure. Persisted, so the provider stays
    /// closed across restarts until someone remediates it.
    CloseUntilRemediated,
    /// Rate limited. The dispatcher holds the provider back until the limit
    /// resets; nothing is persisted.
    BackOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationAction {
    /// A response is under way. Settle it from the stream or by retrieval.
    Settle,
    /// Upstream refused the create request itself with a status proving
    /// processing never started. Release the reservation (ADR-0006).
    ReleaseRejected,
    /// The outcome cannot be shown not to have started generation. Hold the
    /// reservation as sent with an unknown id; it is written off at its
    /// deadline.
    HoldAsUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusDecision {
    pub client: ClientAction,
    pub provider: ProviderAction,
    pub reservation: ReservationAction,
}

#[expect(
    clippy::manual_range_patterns,
    reason = "each status is its own decision; a range would quietly admit a neighbour added later"
)]
pub fn decide(status: u16) -> StatusDecision {
    use ClientAction::*;
    use ProviderAction::*;
    use ReservationAction::*;
    let decision = |client, provider, reservation| StatusDecision {
        client,
        provider,
        reservation,
    };

    match status {
        200..=299 => decision(Relay, Keep, Settle),
        // Provider APIs do not redirect. Whoever answered may not be the
        // provider, and the original request may have been forwarded and
        // processed. Never followed, and the reservation is held (ADR-0005).
        300..=399 => decision(TryNextProvider, CloseUntilRemediated, HoldAsUnknown),
        400 | 422 => decision(ReturnError, Keep, ReleaseRejected),
        // Credentials, payment, or an endpoint that does not exist: the
        // provider is misconfigured or its safety posture has changed. 402 is
        // what a zero-balance OpenRouter account returns for anything paid.
        401 | 402 | 403 | 404 => decision(TryNextProvider, CloseUntilRemediated, ReleaseRejected),
        429 => decision(TryNextProvider, BackOff, ReleaseRejected),
        // Server errors, gateway timeouts, and anything not listed: generation
        // may have started.
        _ => decision(TryNextProvider, Keep, HoldAsUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_relays_and_settles() {
        let d = decide(200);
        assert_eq!(
            (d.client, d.provider, d.reservation),
            (
                ClientAction::Relay,
                ProviderAction::Keep,
                ReservationAction::Settle
            )
        );
    }

    #[test]
    fn exactly_the_listed_refusals_release_the_reservation() {
        for status in 100..600u16 {
            let releases = decide(status).reservation == ReservationAction::ReleaseRejected;
            assert_eq!(
                releases,
                matches!(status, 400 | 401 | 402 | 403 | 404 | 422 | 429),
                "{status}"
            );
        }
    }

    #[test]
    fn redirects_and_server_errors_hold_the_reservation() {
        for status in [301, 302, 307, 308, 405, 409, 413, 500, 502, 503, 504] {
            assert_eq!(
                decide(status).reservation,
                ReservationAction::HoldAsUnknown,
                "{status}"
            );
        }
    }

    #[test]
    fn a_redirect_closes_the_provider() {
        for status in [301, 302, 303, 307, 308] {
            let d = decide(status);
            assert_eq!(d.client, ClientAction::TryNextProvider, "{status}");
            assert_eq!(d.provider, ProviderAction::CloseUntilRemediated, "{status}");
        }
    }

    #[test]
    fn only_a_malformed_request_is_returned_to_the_client() {
        for status in 100..600u16 {
            let returned = decide(status).client == ClientAction::ReturnError;
            assert_eq!(returned, matches!(status, 400 | 422), "{status}");
        }
    }

    #[test]
    fn payment_required_moves_on_and_closes_the_provider() {
        let d = decide(402);
        assert_eq!(d.client, ClientAction::TryNextProvider);
        assert_eq!(d.provider, ProviderAction::CloseUntilRemediated);
    }

    #[test]
    fn a_rate_limit_backs_off_and_server_errors_do_not_close() {
        assert_eq!(decide(429).provider, ProviderAction::BackOff);
        for status in [500, 502, 503, 504] {
            let d = decide(status);
            assert_eq!(d.client, ClientAction::TryNextProvider, "{status}");
            assert_eq!(d.provider, ProviderAction::Keep, "{status}");
        }
    }
}
