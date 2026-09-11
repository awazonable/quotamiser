//! Observing an upstream Responses API event stream, and reading the response
//! object that retrieval returns.
//!
//! The proxy relays the stream's bytes to the client unchanged and watches a
//! copy for the two things settlement needs: the response id, first carried by
//! `response.created`, and the final usage, carried by the event that brings
//! the response to a terminal status. Nothing else is interpreted.
//!
//! The observer is strict about the events it does read. A lifecycle event it
//! cannot parse, a second response id, or terminal events that disagree all
//! fail it permanently, because each means the stream can no longer be the
//! basis for settlement. Retrieval by id is the fallback.

use serde_json::Value;

use crate::sse::{Event, EventSplitter, FramingError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseSummary {
    pub id: String,
    pub status: String,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    /// `None` when absent, null, or not a pair of integers. Negative values
    /// are passed through for the ledger's validation to reject.
    pub usage: Option<Usage>,
}

impl ResponseSummary {
    /// Reads a response object, as embedded in lifecycle events or returned
    /// by `GET /v1/responses/{id}`.
    pub fn from_object(object: &Value) -> Option<Self> {
        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
        let usage = object.get("usage").and_then(|usage| {
            Some(Usage {
                input_tokens: usage.get("input_tokens")?.as_i64()?,
                output_tokens: usage.get("output_tokens")?.as_i64()?,
            })
        });
        Some(Self {
            id: text("id")?,
            status: text("status")?,
            model: text("model"),
            service_tier: text("service_tier"),
            usage,
        })
    }

    /// A status the response will not leave.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "completed" | "incomplete" | "failed" | "cancelled"
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    ResponseId(String),
    Terminal(ResponseSummary),
    /// An `error` event. The response may still exist upstream; retrieve it.
    StreamError {
        code: Option<String>,
        message: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ObserveFailure {
    #[error(transparent)]
    Framing(#[from] FramingError),
    #[error("a response lifecycle event could not be read")]
    MalformedLifecycleEvent,
    #[error("the stream named more than one response")]
    ConflictingResponseIds,
    #[error("the stream reported more than one final state for the response")]
    ConflictingTerminals,
}

const LIFECYCLE_EVENTS: [&str; 7] = [
    "response.created",
    "response.queued",
    "response.in_progress",
    "response.completed",
    "response.incomplete",
    "response.failed",
    "response.cancelled",
];

#[derive(Debug, Default)]
pub struct StreamObserver {
    splitter: EventSplitter,
    response_id: Option<String>,
    terminal: Option<ResponseSummary>,
    failure: Option<ObserveFailure>,
}

impl StreamObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a copy of relayed bytes. Returns what they revealed; nothing
    /// once the observer has failed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Observation> {
        let mut observations = Vec::new();
        if self.failure.is_some() {
            return observations;
        }
        for event in self.splitter.push(bytes) {
            if let Err(failure) = self.observe(event, &mut observations) {
                self.failure = Some(failure);
                return observations;
            }
        }
        if let Some(framing) = self.splitter.failure() {
            self.failure = Some(framing.into());
        }
        observations
    }

    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    pub fn terminal(&self) -> Option<&ResponseSummary> {
        self.terminal.as_ref()
    }

    pub fn failure(&self) -> Option<ObserveFailure> {
        self.failure
    }

    fn observe(&mut self, event: Event, out: &mut Vec<Observation>) -> Result<(), ObserveFailure> {
        let name = match &event.event {
            Some(name) => name.clone(),
            None => serde_json::from_str::<Value>(&event.data)
                .ok()
                .and_then(|v| v.get("type")?.as_str().map(str::to_string))
                .unwrap_or_default(),
        };

        if name == "error" {
            let value = serde_json::from_str::<Value>(&event.data).unwrap_or(Value::Null);
            let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
            out.push(Observation::StreamError {
                code: text("code"),
                message: text("message"),
            });
            return Ok(());
        }
        if !LIFECYCLE_EVENTS.contains(&name.as_str()) {
            return Ok(());
        }

        let value: Value = serde_json::from_str(&event.data)
            .map_err(|_| ObserveFailure::MalformedLifecycleEvent)?;
        if value
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t != name)
        {
            return Err(ObserveFailure::MalformedLifecycleEvent);
        }
        let summary = value
            .get("response")
            .and_then(ResponseSummary::from_object)
            .ok_or(ObserveFailure::MalformedLifecycleEvent)?;

        match &self.response_id {
            None => {
                self.response_id = Some(summary.id.clone());
                out.push(Observation::ResponseId(summary.id.clone()));
            }
            Some(id) if *id != summary.id => return Err(ObserveFailure::ConflictingResponseIds),
            Some(_) => {}
        }

        if summary.is_terminal() {
            match &self.terminal {
                None => {
                    self.terminal = Some(summary.clone());
                    out.push(Observation::Terminal(summary));
                }
                Some(previous) if *previous != summary => {
                    return Err(ObserveFailure::ConflictingTerminals);
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn lifecycle(name: &str, status: &str, usage: Value) -> String {
        let data = json!({
            "type": name,
            "response": {
                "id": "resp_1",
                "object": "response",
                "status": status,
                "model": "gpt-5.6-terra",
                "service_tier": "default",
                "usage": usage,
            },
            "sequence_number": 0,
        });
        format!("event: {name}\ndata: {data}\n\n")
    }

    fn completed(input: i64, output: i64) -> String {
        lifecycle(
            "response.completed",
            "completed",
            json!({"input_tokens": input, "output_tokens": output, "total_tokens": input + output}),
        )
    }

    #[test]
    fn the_id_and_the_final_usage_are_observed() {
        let mut observer = StreamObserver::new();
        let created =
            observer.push(lifecycle("response.created", "in_progress", Value::Null).as_bytes());
        assert_eq!(created, [Observation::ResponseId("resp_1".into())]);

        observer.push(b"event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n");
        let done = observer.push(completed(9, 5).as_bytes());
        let summary = ResponseSummary {
            id: "resp_1".into(),
            status: "completed".into(),
            model: Some("gpt-5.6-terra".into()),
            service_tier: Some("default".into()),
            usage: Some(Usage {
                input_tokens: 9,
                output_tokens: 5,
            }),
        };
        assert_eq!(done, [Observation::Terminal(summary.clone())]);
        assert_eq!(observer.terminal(), Some(&summary));
        assert_eq!(observer.response_id(), Some("resp_1"));
        assert_eq!(observer.failure(), None);
    }

    #[test]
    fn an_incomplete_response_is_terminal() {
        let mut observer = StreamObserver::new();
        observer.push(
            lifecycle(
                "response.incomplete",
                "incomplete",
                json!({"input_tokens": 3, "output_tokens": 4000}),
            )
            .as_bytes(),
        );
        assert_eq!(
            observer.terminal().unwrap().usage,
            Some(Usage {
                input_tokens: 3,
                output_tokens: 4000
            })
        );
    }

    #[test]
    fn events_split_across_packets_are_still_observed() {
        let mut observer = StreamObserver::new();
        let text = completed(1, 2);
        let (head, tail) = text.as_bytes().split_at(text.len() / 2);
        assert!(observer.push(head).is_empty());
        assert_eq!(observer.push(tail).len(), 2);
    }

    #[test]
    fn non_lifecycle_events_are_not_parsed() {
        let mut observer = StreamObserver::new();
        assert!(
            observer
                .push(b"event: response.output_text.delta\ndata: not json at all\n\n")
                .is_empty()
        );
        assert_eq!(observer.failure(), None);
    }

    #[test]
    fn an_error_event_is_reported() {
        let mut observer = StreamObserver::new();
        let observed = observer.push(b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"boom\"}\n\n");
        assert_eq!(
            observed,
            [Observation::StreamError {
                code: Some("server_error".into()),
                message: Some("boom".into())
            }]
        );
    }

    #[test]
    fn an_unreadable_lifecycle_event_fails_the_observer() {
        let mut observer = StreamObserver::new();
        observer.push(b"event: response.completed\ndata: {truncated\n\n");
        assert_eq!(
            observer.failure(),
            Some(ObserveFailure::MalformedLifecycleEvent)
        );
        assert!(observer.push(completed(1, 1).as_bytes()).is_empty());
    }

    #[test]
    fn an_event_name_that_disagrees_with_its_type_is_malformed() {
        let mut observer = StreamObserver::new();
        let text =
            completed(1, 1).replacen("event: response.completed", "event: response.created", 1);
        observer.push(text.as_bytes());
        assert_eq!(
            observer.failure(),
            Some(ObserveFailure::MalformedLifecycleEvent)
        );
    }

    #[test]
    fn a_second_response_id_fails_the_observer() {
        let mut observer = StreamObserver::new();
        observer.push(lifecycle("response.created", "in_progress", Value::Null).as_bytes());
        observer.push(completed(1, 1).replace("resp_1", "resp_2").as_bytes());
        assert_eq!(
            observer.failure(),
            Some(ObserveFailure::ConflictingResponseIds)
        );
        assert_eq!(observer.terminal(), None);
    }

    #[test]
    fn disagreeing_terminal_events_fail_the_observer() {
        let mut observer = StreamObserver::new();
        observer.push(completed(1, 1).as_bytes());
        observer.push(completed(1, 1).as_bytes());
        assert_eq!(
            observer.failure(),
            None,
            "a repeated identical terminal is harmless"
        );
        observer.push(completed(1, 999).as_bytes());
        assert_eq!(
            observer.failure(),
            Some(ObserveFailure::ConflictingTerminals)
        );
    }

    #[test]
    fn a_framing_failure_fails_the_observer() {
        let mut observer = StreamObserver::new();
        observer.push(b"data: \xff\n\n");
        assert_eq!(
            observer.failure(),
            Some(ObserveFailure::Framing(FramingError::InvalidUtf8))
        );
    }

    #[test]
    fn a_retrieved_response_object_is_read() {
        let body = json!({
            "id": "resp_9", "object": "response", "status": "cancelled", "model": "gpt-5.6-terra",
            "usage": {"input_tokens": 35, "output_tokens": 5337, "total_tokens": 5372},
        });
        let summary = ResponseSummary::from_object(&body).unwrap();
        assert!(summary.is_terminal());
        assert_eq!(summary.service_tier, None);
        assert_eq!(
            summary.usage,
            Some(Usage {
                input_tokens: 35,
                output_tokens: 5337
            })
        );
    }

    #[test]
    fn usage_that_is_not_integers_reads_as_absent() {
        let body = json!({"id": "r", "status": "completed", "usage": {"input_tokens": "9", "output_tokens": 5}});
        assert_eq!(ResponseSummary::from_object(&body).unwrap().usage, None);
        let in_progress = json!({"id": "r", "status": "in_progress", "usage": null});
        assert!(
            !ResponseSummary::from_object(&in_progress)
                .unwrap()
                .is_terminal()
        );
    }
}
