//! Reservation states and the transitions allowed between them.
//!
//! Every transition applies to a whole reservation group: when a reservation
//! crosses a daily reset it is represented by one row per epoch sharing a
//! `root_id`, and those rows must never disagree about their state.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Reserved,
    Dispatching,
    DispatchedWithId,
    DispatchedIdUnknown,
    Settled,
    ConsumedUnrecoverable,
    ReleasedUnsent,
}

impl State {
    pub const ALL: [State; 7] = [
        State::Reserved,
        State::Dispatching,
        State::DispatchedWithId,
        State::DispatchedIdUnknown,
        State::Settled,
        State::ConsumedUnrecoverable,
        State::ReleasedUnsent,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            State::Reserved => "RESERVED",
            State::Dispatching => "DISPATCHING",
            State::DispatchedWithId => "DISPATCHED_WITH_ID",
            State::DispatchedIdUnknown => "DISPATCHED_ID_UNKNOWN",
            State::Settled => "SETTLED",
            State::ConsumedUnrecoverable => "CONSUMED_UNRECOVERABLE",
            State::ReleasedUnsent => "RELEASED_UNSENT",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        State::ALL.into_iter().find(|state| state.as_str() == s)
    }

    /// Carries its liability in `pool_epoch.reserved`.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            State::Reserved
                | State::Dispatching
                | State::DispatchedWithId
                | State::DispatchedIdUnknown
        )
    }

    /// May have reached the upstream provider, so its liability can only be
    /// removed by evidence of what it actually consumed.
    pub fn possibly_sent(self) -> bool {
        matches!(
            self,
            State::Dispatching | State::DispatchedWithId | State::DispatchedIdUnknown
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    BeginDispatch,
    AttachResponseId,
    MarkIdUnknown,
    Settle,
    Unrecoverable,
    ReleaseUnsent,
}

impl Transition {
    pub fn target(self) -> State {
        match self {
            Transition::BeginDispatch => State::Dispatching,
            Transition::AttachResponseId => State::DispatchedWithId,
            Transition::MarkIdUnknown => State::DispatchedIdUnknown,
            Transition::Settle => State::Settled,
            Transition::Unrecoverable => State::ConsumedUnrecoverable,
            Transition::ReleaseUnsent => State::ReleasedUnsent,
        }
    }

    pub fn allowed_from(self, from: State) -> bool {
        use State::*;
        match self {
            Transition::BeginDispatch => from == Reserved,
            Transition::AttachResponseId => from == Dispatching,
            Transition::MarkIdUnknown => from == Dispatching,
            // Settling needs authoritative usage, which cannot be obtained
            // without the response id; an id-unknown request can only be
            // written off.
            Transition::Settle => matches!(from, Dispatching | DispatchedWithId),
            Transition::Unrecoverable => from.possibly_sent(),
            // Only a request that provably never reached the HTTP stack may
            // be released without evidence.
            Transition::ReleaseUnsent => from == Reserved,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for state in State::ALL {
            assert_eq!(State::parse(state.as_str()), Some(state));
        }
        assert_eq!(State::parse("NOPE"), None);
    }

    #[test]
    fn terminal_states_allow_nothing() {
        let transitions = [
            Transition::BeginDispatch,
            Transition::AttachResponseId,
            Transition::MarkIdUnknown,
            Transition::Settle,
            Transition::Unrecoverable,
            Transition::ReleaseUnsent,
        ];
        for terminal in [
            State::Settled,
            State::ConsumedUnrecoverable,
            State::ReleasedUnsent,
        ] {
            assert!(!terminal.is_active());
            for t in transitions {
                assert!(!t.allowed_from(terminal), "{t:?} from {terminal:?}");
            }
        }
    }

    #[test]
    fn a_possibly_sent_request_is_never_released_without_evidence() {
        for state in State::ALL.into_iter().filter(|s| s.possibly_sent()) {
            assert!(!Transition::ReleaseUnsent.allowed_from(state));
        }
    }

    #[test]
    fn id_unknown_cannot_settle() {
        assert!(!Transition::Settle.allowed_from(State::DispatchedIdUnknown));
        assert!(Transition::Unrecoverable.allowed_from(State::DispatchedIdUnknown));
    }
}
