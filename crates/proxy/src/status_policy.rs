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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationAction {
    /// A response is under way. Settle it from the stream or by retrieval.
    Settle,
    /// No response id exists and the request may have been processed. Hold
    /// the reservation as sent with an unknown id; it is written off at its
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
        // processed. Never followed (ADR-0005).
        300..=399 => decision(TryNextProvider, CloseUntilRemediated, HoldAsUnknown),
        400 | 422 => decision(ReturnError, Keep, HoldAsUnknown),
        // Credentials, payment, or an endpoint that does not exist: the
        // provider is misconfigured or its safety posture has changed. 402 is
        // what a zero-balance OpenRouter account returns for anything paid.
        401 | 402 | 403 | 404 => decision(TryNextProvider, CloseUntilRemediated, HoldAsUnknown),
        429 => decision(TryNextProvider, Keep, HoldAsUnknown),
        // Ambiguous: timeouts at a gateway, server errors, and anything not
        // listed. The request may have been processed.
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
    fn a_redirect_closes_the_provider_and_holds_the_reservation() {
        for status in [301, 302, 303, 307, 308] {
            let d = decide(status);
            assert_eq!(d.client, ClientAction::TryNextProvider, "{status}");
            assert_eq!(d.provider, ProviderAction::CloseUntilRemediated, "{status}");
            assert_eq!(d.reservation, ReservationAction::HoldAsUnknown, "{status}");
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
    fn rate_limits_and_server_errors_move_on_without_closing() {
        for status in [429, 500, 502, 503, 504] {
            let d = decide(status);
            assert_eq!(d.client, ClientAction::TryNextProvider, "{status}");
            assert_eq!(d.provider, ProviderAction::Keep, "{status}");
        }
    }

    #[test]
    fn only_success_releases_the_reservation_to_settlement() {
        for status in 100..600u16 {
            let settles = decide(status).reservation == ReservationAction::Settle;
            assert_eq!(settles, (200..300).contains(&status), "{status}");
        }
    }
}
