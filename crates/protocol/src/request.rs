//! A client's `POST /v1/responses` body: parsed once, checked against the v1
//! allowlist, and normalized into the body that is reserved for and sent
//! upstream.
//!
//! Deny by default. A field, input item, content part, tool type or
//! enumerated value that is not listed here is refused, standard Responses
//! API features included: an unlisted feature may bill outside the token
//! grant, or add input the liability cannot see. Every refusal names the
//! offending JSON path and says what to change.
//!
//! Normalization removes `id` and `status` from input items, so history
//! always travels by content and never by reference to stored items. It
//! removes `client_metadata`, where Codex puts workspace details such as git
//! remote URLs. It replaces `store`, `stream` and `service_tier` with the
//! values the upstream request needs. Field order is kept, and nothing else is
//! rewritten.
//!
//! Accepting a tool type here says nothing about how it is billed. Whether
//! requests carrying `function`, `custom` or `namespace` tools draw on the
//! complimentary grant is a measured observation, recorded with its limits in
//! the requirements, not a property this module establishes.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::error_body;

/// Changes whenever what is accepted changes.
pub const ALLOWLIST_VERSION: &str = "responses-ingress/1";

/// The service tier every upstream request is sent with, and the one
/// settlement expects the terminal event to report. Sending `auto`, or
/// nothing, reports `auto` until the terminal event and `default` in it, which
/// would fail validation on the first settlement.
pub const UPSTREAM_SERVICE_TIER: &str = "default";

/// The hint for fields only Codex's built-in `openai` provider sends.
const CODEX_PROVIDER_HINT: &str = "If this request comes from Codex, configure QuotaMiser as a custom model provider rather than overriding the built-in `openai` provider.";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Rejection {
    pub code: RejectionCode,
    /// The offending JSON path, such as `input[3].content[0].image_url`.
    pub param: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionCode {
    InvalidJson,
    DuplicateKey,
    UnsupportedParameter,
    MissingParameter,
    InvalidValue,
    ServerSideState,
    HostedTool,
    UnsupportedInputItem,
    RemoteReference,
}

impl RejectionCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::DuplicateKey => "duplicate_key",
            Self::UnsupportedParameter => "unsupported_parameter",
            Self::MissingParameter => "missing_required_parameter",
            Self::InvalidValue => "invalid_value",
            Self::ServerSideState => "server_side_state_unsupported",
            Self::HostedTool => "hosted_tool_unsupported",
            Self::UnsupportedInputItem => "unsupported_input_item",
            Self::RemoteReference => "remote_reference_unsupported",
        }
    }
}

impl Rejection {
    fn new(code: RejectionCode, param: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code,
            param: param.map(str::to_owned),
            message: message.into(),
        }
    }

    /// The body of the 400 returned to the client.
    pub fn error_body(&self) -> Value {
        error_body::invalid_request_body(&self.message, self.param.as_deref(), self.code.as_str())
    }
}

/// A request that passed the allowlist, in normalized form.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRequest {
    fields: Map<String, Value>,
    model: String,
    max_output_tokens: Option<u64>,
}

/// A single text input with at most a single text instructions, and nothing
/// else that adds input: no tools, tool choice or structured output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlatText<'a> {
    pub instructions: Option<&'a str>,
    pub input: &'a str,
}

/// Which tool shapes a request carries, wherever they are declared: at the top
/// level, inside a namespace, or in an `additional_tools` input item.
///
/// A provider is chosen against this before anything is sent. Sending a shape
/// the provider refuses wastes a request on a provider whose allowance is
/// counted in requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolUsage {
    pub function: bool,
    pub custom: bool,
    pub namespace: bool,
    pub additional_tools: bool,
}

/// Fields `POST /v1/responses/input_tokens` takes that can change the count.
/// `text` is among them: a JSON Schema output format is part of the input.
const COUNTED_FIELDS: [&str; 8] = [
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "text",
];

impl CreateRequest {
    pub fn parse(body: &[u8]) -> Result<Self, Rejection> {
        let Value::Object(object) = parse_strict(body)? else {
            return Err(Rejection::new(
                RejectionCode::InvalidJson,
                None,
                "The request body must be a JSON object.",
            ));
        };

        let mut fields = Map::new();
        let mut stream = None;
        for (key, value) in object {
            let path = key.clone();
            let path = path.as_str();
            match path {
                "model" => {
                    fields.insert(key, non_empty_string(value, path)?);
                }
                "input" => {
                    fields.insert(key, input(value, path)?);
                }
                "instructions" | "prompt_cache_key" => {
                    fields.insert(key, string(value, path)?);
                }
                "tools" => {
                    fields.insert(key, tool_list(value, path)?);
                }
                "tool_choice" => {
                    fields.insert(key, tool_choice(value, path)?);
                }
                "parallel_tool_calls" => {
                    fields.insert(key, boolean(value, path)?);
                }
                "reasoning" => {
                    fields.insert(key, object_of(&REASONING, value, path)?);
                }
                "text" => {
                    fields.insert(key, object_of(&TEXT, value, path)?);
                }
                "include" => {
                    fields.insert(key, include(value, path)?);
                }
                "max_output_tokens" => {
                    fields.insert(key, positive_integer(value, path)?);
                }
                "temperature" | "top_p" => {
                    fields.insert(key, number(value, path)?);
                }
                "metadata" => {
                    fields.insert(key, metadata(value, path)?);
                }
                // `{"ttl": "30m"}` is the only supported value and the default.
                "prompt_cache_options" => {
                    object_of(&PROMPT_CACHE_OPTIONS, value, path)?;
                }
                "client_metadata" => {}
                // Replaced by the values the upstream request needs.
                "store" => {
                    boolean(value, path)?;
                }
                "service_tier" => {
                    string(value, path)?;
                }
                "stream" => {
                    stream = Some(value);
                }
                "background" => match value {
                    Value::Null | Value::Bool(false) => {}
                    _ => {
                        return Err(Rejection::new(
                            RejectionCode::InvalidValue,
                            Some(path),
                            "`background` is managed by QuotaMiser. Omit it.",
                        ));
                    }
                },
                "previous_response_id" | "conversation" | "prompt" => {
                    if !value.is_null() {
                        return Err(Rejection::new(
                            RejectionCode::ServerSideState,
                            Some(path),
                            format!(
                                "`{path}` refers to state stored upstream, whose size cannot be counted before sending. Send the full conversation in `input` instead."
                            ),
                        ));
                    }
                }
                _ => return Err(unsupported_parameter(path)),
            }
        }

        match stream {
            Some(Value::Bool(true)) => {}
            Some(_) => {
                return Err(Rejection::new(
                    RejectionCode::InvalidValue,
                    Some("stream"),
                    "QuotaMiser relays streamed responses only. Send `stream: true`.",
                ));
            }
            None => {
                return Err(Rejection::new(
                    RejectionCode::MissingParameter,
                    Some("stream"),
                    "QuotaMiser relays streamed responses only. Send `stream: true`.",
                ));
            }
        }
        let Some(Value::String(model)) = fields.get("model") else {
            return Err(missing_parameter("model"));
        };
        let model = model.clone();
        if !fields.contains_key("input") {
            return Err(missing_parameter("input"));
        }
        let max_output_tokens = fields.get("max_output_tokens").and_then(Value::as_u64);

        Ok(Self {
            fields,
            model,
            max_output_tokens,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    /// The request in the flat form the structural input bound covers, or
    /// `None` when only the provider's count will do.
    pub fn flat_text(&self) -> Option<FlatText<'_>> {
        let Some(Value::String(input)) = self.fields.get("input") else {
            return None;
        };
        let instructions = match self.fields.get("instructions") {
            None => None,
            Some(Value::String(text)) => Some(text.as_str()),
            Some(_) => return None,
        };
        let has_tools = self
            .fields
            .get("tools")
            .is_some_and(|tools| tools.as_array().is_none_or(|tools| !tools.is_empty()));
        let formats_output = self
            .fields
            .get("text")
            .and_then(|text| text.get("format"))
            .is_some_and(|format| format.get("type").and_then(Value::as_str) != Some("text"));
        if has_tools || self.fields.contains_key("tool_choice") || formats_output {
            return None;
        }
        Some(FlatText {
            instructions,
            input,
        })
    }

    /// The canonical body sent to OpenAI: the normalized client fields, then
    /// the fields QuotaMiser fixes. Background mode and storage let a response
    /// be cancelled and settled after the client disconnects (ADR-0002), and
    /// the service tier is the one the settlement check expects.
    pub fn openai_body(&self) -> Vec<u8> {
        let mut body = self.fields.clone();
        body.insert("background".into(), Value::Bool(true));
        body.insert("store".into(), Value::Bool(true));
        body.insert("stream".into(), Value::Bool(true));
        body.insert(
            "service_tier".into(),
            Value::String(UPSTREAM_SERVICE_TIER.into()),
        );
        serde_json::to_vec(&Value::Object(body)).expect("a JSON value always serializes")
    }

    /// The canonical body for OpenRouter, whose free allowance is counted in
    /// requests rather than tokens.
    ///
    /// Measured on 2026-09-12: OpenRouter's Responses endpoint refuses
    /// `store: true` outright ("expected false"), so the retrieval-based
    /// settlement OpenAI needs has nothing to work with here — and needs
    /// nothing, since no token is being reserved. Fields whose behaviour was
    /// not measured there are left out rather than guessed at: the request is
    /// reduced to what a measurement showed the endpoint accepts.
    pub fn openrouter_body(&self, model: &str) -> Vec<u8> {
        let mut body = Map::new();
        body.insert("model".into(), Value::String(model.to_owned()));
        for name in [
            "instructions",
            "input",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "max_output_tokens",
            "temperature",
            "top_p",
        ] {
            if let Some(value) = self.fields.get(name) {
                body.insert(name.to_owned(), value.clone());
            }
        }
        // Only the effort was measured; the summary and the cross-turn context
        // are OpenAI's own.
        if let Some(Value::Object(reasoning)) = self.fields.get("reasoning")
            && let Some(effort) = reasoning.get("effort")
        {
            body.insert(
                "reasoning".into(),
                Value::Object(Map::from_iter([("effort".to_owned(), effort.clone())])),
            );
        }
        // A structured output format travels; the verbosity knob is OpenAI's.
        if let Some(Value::Object(text)) = self.fields.get("text")
            && let Some(format) = text.get("format")
        {
            body.insert(
                "text".into(),
                Value::Object(Map::from_iter([("format".to_owned(), format.clone())])),
            );
        }
        body.insert("stream".into(), Value::Bool(true));
        body.insert("store".into(), Value::Bool(false));
        serde_json::to_vec(&Value::Object(body)).expect("a JSON value always serializes")
    }

    /// Whether the client asked for a constrained output format, which not
    /// every free model can do.
    pub fn needs_structured_output(&self) -> bool {
        self.fields
            .get("text")
            .and_then(|text| text.get("format"))
            .and_then(|format| format.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind != "text")
    }

    /// Which tool shapes this request carries.
    pub fn tool_usage(&self) -> ToolUsage {
        let mut usage = ToolUsage::default();
        if let Some(Value::Array(tools)) = self.fields.get("tools") {
            absorb_tools(tools, &mut usage);
        }
        if let Some(Value::Array(items)) = self.fields.get("input") {
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                    continue;
                }
                usage.additional_tools = true;
                if let Some(Value::Array(tools)) = item.get("tools") {
                    absorb_tools(tools, &mut usage);
                }
            }
        }
        usage
    }

    /// Bodies for `POST /v1/responses/input_tokens`. The sum of their counts
    /// bounds the input the request consumes.
    ///
    /// The first body is the request itself. Measured on 2026-09-11, the count
    /// matches generation for every accepted shape except tools declared in an
    /// `additional_tools` input item: generation renders those tools once more
    /// than the count does. So each `additional_tools` item adds a body that
    /// declares its tools at the top level, in place of every such item, with
    /// the rest of the request unchanged. That body's count covers the tools'
    /// rendering once more, plus the rest of the input again as margin.
    pub fn input_count_bodies(&self) -> Vec<Value> {
        let counted: Map<String, Value> = COUNTED_FIELDS
            .iter()
            .filter_map(|&name| self.fields.get(name).map(|v| (name.to_owned(), v.clone())))
            .collect();
        let mut bodies = vec![Value::Object(counted.clone())];

        let Some(Value::Array(items)) = self.fields.get("input") else {
            return bodies;
        };
        let is_additional_tools =
            |item: &Value| item.get("type").and_then(Value::as_str) == Some("additional_tools");
        let remaining: Vec<Value> = items
            .iter()
            .filter(|item| !is_additional_tools(item))
            .cloned()
            .collect();
        for item in items.iter().filter(|item| is_additional_tools(item)) {
            let mut body = counted.clone();
            body.insert("input".into(), Value::Array(remaining.clone()));
            body.insert(
                "tools".into(),
                item.get("tools")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new())),
            );
            // A specific tool choice may name a tool this body does not declare.
            if body.get("tool_choice").is_some_and(Value::is_object) {
                body.shift_remove("tool_choice");
            }
            bodies.push(Value::Object(body));
        }
        bodies
    }
}

fn absorb_tools(tools: &[Value], usage: &mut ToolUsage) {
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => usage.function = true,
            Some("custom") => usage.custom = true,
            Some("namespace") => {
                usage.namespace = true;
                if let Some(Value::Array(nested)) = tool.get("tools") {
                    absorb_tools(nested, usage);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Strict parsing: duplicate keys are refused at any depth.

struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("a number must be finite"))
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key `{key}`")));
            }
            let Strict(value) = map.next_value()?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

fn parse_strict(body: &[u8]) -> Result<Value, Rejection> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let parsed = Strict::deserialize(&mut deserializer).and_then(|Strict(value)| {
        deserializer.end()?;
        Ok(value)
    });
    parsed.map_err(|error| {
        // The visitor accepts every JSON value, so the only data error is a
        // duplicate key; everything else is a syntax error.
        let code = if error.is_data() {
            RejectionCode::DuplicateKey
        } else {
            RejectionCode::InvalidJson
        };
        Rejection::new(
            code,
            None,
            format!("The request body is not acceptable JSON: {error}."),
        )
    })
}

// ---------------------------------------------------------------------------
// Object shapes.

type Check = fn(Value, &str) -> Result<Value, Rejection>;

#[derive(Clone, Copy)]
enum Rule {
    /// Validated, then kept.
    Keep(Check),
    /// Accepted, and removed from the normalized body.
    Strip,
}

struct Shape {
    /// What the object is, for messages.
    what: &'static str,
    fields: &'static [(&'static str, Rule)],
    required: &'static [&'static str],
}

impl Shape {
    fn apply(
        &self,
        object: Map<String, Value>,
        path: &str,
    ) -> Result<Map<String, Value>, Rejection> {
        let mut normalized = Map::new();
        for (key, value) in object {
            let field_path = join(path, &key);
            match self.fields.iter().find(|(name, _)| *name == key) {
                Some((_, Rule::Keep(check))) => {
                    let value = check(value, &field_path)?;
                    normalized.insert(key, value);
                }
                Some((_, Rule::Strip)) => {}
                None => return Err(unsupported_field(&field_path, self.what, &key)),
            }
        }
        if let Some(name) = self
            .required
            .iter()
            .find(|name| !normalized.contains_key(**name))
        {
            return Err(missing_parameter(&join(path, name)));
        }
        Ok(normalized)
    }
}

const TYPE: (&str, Rule) = ("type", Rule::Keep(string));
const ID: (&str, Rule) = ("id", Rule::Strip);
const STATUS: (&str, Rule) = ("status", Rule::Strip);

const MESSAGE: Shape = Shape {
    what: "message",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("role", Rule::Keep(message_role)),
        ("content", Rule::Keep(message_content)),
        ("phase", Rule::Keep(message_phase)),
    ],
    required: &["role", "content"],
};

/// Replayed with its encrypted content, which the provider counts.
const REASONING_ITEM: Shape = Shape {
    what: "reasoning item",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("summary", Rule::Keep(summary_parts)),
        ("content", Rule::Keep(reasoning_text_parts)),
        ("encrypted_content", Rule::Keep(non_empty_string)),
    ],
    required: &["encrypted_content"],
};

const FUNCTION_CALL: Shape = Shape {
    what: "function_call item",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("call_id", Rule::Keep(non_empty_string)),
        ("name", Rule::Keep(non_empty_string)),
        ("namespace", Rule::Keep(string)),
        ("arguments", Rule::Keep(string)),
    ],
    required: &["call_id", "name", "arguments"],
};

const FUNCTION_CALL_OUTPUT: Shape = Shape {
    what: "function_call_output item",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("call_id", Rule::Keep(non_empty_string)),
        ("name", Rule::Keep(string)),
        ("namespace", Rule::Keep(string)),
        ("output", Rule::Keep(tool_output)),
    ],
    required: &["call_id", "output"],
};

const CUSTOM_TOOL_CALL: Shape = Shape {
    what: "custom_tool_call item",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("call_id", Rule::Keep(non_empty_string)),
        ("name", Rule::Keep(non_empty_string)),
        ("namespace", Rule::Keep(string)),
        ("input", Rule::Keep(string)),
    ],
    required: &["call_id", "name", "input"],
};

const CUSTOM_TOOL_CALL_OUTPUT: Shape = Shape {
    what: "custom_tool_call_output item",
    fields: &[
        TYPE,
        ID,
        STATUS,
        ("call_id", Rule::Keep(non_empty_string)),
        ("name", Rule::Keep(string)),
        ("output", Rule::Keep(tool_output)),
    ],
    required: &["call_id", "output"],
};

/// Codex declares tools this way for its Lite models.
const ADDITIONAL_TOOLS: Shape = Shape {
    what: "additional_tools item",
    fields: &[
        TYPE,
        ID,
        ("role", Rule::Keep(developer_role)),
        ("tools", Rule::Keep(tool_list)),
    ],
    required: &["role", "tools"],
};

const INPUT_TEXT: Shape = Shape {
    what: "input_text part",
    fields: &[TYPE, ("text", Rule::Keep(string))],
    required: &["text"],
};

const OUTPUT_TEXT: Shape = Shape {
    what: "output_text part",
    fields: &[
        TYPE,
        ("text", Rule::Keep(string)),
        ("annotations", Rule::Strip),
        ("logprobs", Rule::Strip),
    ],
    required: &["text"],
};

const REFUSAL: Shape = Shape {
    what: "refusal part",
    fields: &[TYPE, ("refusal", Rule::Keep(string))],
    required: &["refusal"],
};

/// Inline images only: a remote URL could serve different content when
/// counted and when generated.
const INPUT_IMAGE: Shape = Shape {
    what: "input_image part",
    fields: &[
        TYPE,
        ("image_url", Rule::Keep(inline_image_url)),
        ("detail", Rule::Keep(image_detail)),
    ],
    required: &["image_url"],
};

const SUMMARY_TEXT: Shape = Shape {
    what: "summary_text part",
    fields: &[TYPE, ("text", Rule::Keep(string))],
    required: &["text"],
};

const REASONING_TEXT: Shape = Shape {
    what: "reasoning_text part",
    fields: &[TYPE, ("text", Rule::Keep(string))],
    required: &["text"],
};

const FUNCTION_TOOL: Shape = Shape {
    what: "function tool",
    fields: &[
        TYPE,
        ("name", Rule::Keep(non_empty_string)),
        ("description", Rule::Keep(string)),
        ("parameters", Rule::Keep(json_object)),
        ("strict", Rule::Keep(boolean_or_null)),
        ("defer_loading", Rule::Keep(not_deferred)),
    ],
    required: &["name"],
};

const CUSTOM_TOOL: Shape = Shape {
    what: "custom tool",
    fields: &[
        TYPE,
        ("name", Rule::Keep(non_empty_string)),
        ("description", Rule::Keep(string)),
        ("format", Rule::Keep(custom_tool_format)),
        ("defer_loading", Rule::Keep(not_deferred)),
    ],
    required: &["name"],
};

const NAMESPACE_TOOL: Shape = Shape {
    what: "namespace tool",
    fields: &[
        TYPE,
        ("name", Rule::Keep(non_empty_string)),
        ("description", Rule::Keep(string)),
        ("tools", Rule::Keep(namespace_members)),
    ],
    required: &["name", "tools"],
};

const TEXT_FORMAT: Shape = Shape {
    what: "custom tool format",
    fields: &[TYPE],
    required: &[],
};

const GRAMMAR_FORMAT: Shape = Shape {
    what: "grammar format",
    fields: &[
        TYPE,
        ("syntax", Rule::Keep(grammar_syntax)),
        ("definition", Rule::Keep(string)),
    ],
    required: &["syntax", "definition"],
};

const NAMED_TOOL_CHOICE: Shape = Shape {
    what: "tool_choice",
    fields: &[TYPE, ("name", Rule::Keep(non_empty_string))],
    required: &["name"],
};

const REASONING: Shape = Shape {
    what: "reasoning",
    fields: &[
        ("effort", Rule::Keep(reasoning_effort)),
        ("summary", Rule::Keep(reasoning_summary)),
        ("context", Rule::Keep(reasoning_context)),
    ],
    required: &[],
};

const TEXT: Shape = Shape {
    what: "text",
    fields: &[
        ("verbosity", Rule::Keep(verbosity)),
        ("format", Rule::Keep(output_format)),
    ],
    required: &[],
};

const PLAIN_OUTPUT_FORMAT: Shape = Shape {
    what: "text.format",
    fields: &[TYPE],
    required: &[],
};

const JSON_SCHEMA_FORMAT: Shape = Shape {
    what: "json_schema format",
    fields: &[
        TYPE,
        ("name", Rule::Keep(non_empty_string)),
        ("schema", Rule::Keep(json_object)),
        ("strict", Rule::Keep(boolean_or_null)),
        ("description", Rule::Keep(string)),
    ],
    required: &["name", "schema"],
};

const PROMPT_CACHE_OPTIONS: Shape = Shape {
    what: "prompt_cache_options",
    fields: &[("ttl", Rule::Keep(cache_ttl))],
    required: &[],
};

// ---------------------------------------------------------------------------
// Checks.

fn input(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::String(_) => Ok(value),
        Value::Array(items) => each(items, path, input_item),
        _ => Err(invalid(path, "must be a string or an array of input items")),
    }
}

fn input_item(value: Value, path: &str) -> Result<Value, Rejection> {
    let Value::Object(object) = value else {
        return Err(invalid(path, "must be an object"));
    };
    // An easy input message may leave out its type.
    let kind = match object.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        None if object.contains_key("role") => "message".to_owned(),
        Some(_) => return Err(invalid(&join(path, "type"), "must be a string")),
        None => return Err(missing_parameter(&join(path, "type"))),
    };
    let shape = match kind.as_str() {
        "message" => &MESSAGE,
        "reasoning" => &REASONING_ITEM,
        "function_call" => &FUNCTION_CALL,
        "function_call_output" => &FUNCTION_CALL_OUTPUT,
        "custom_tool_call" => &CUSTOM_TOOL_CALL,
        "custom_tool_call_output" => &CUSTOM_TOOL_CALL_OUTPUT,
        "additional_tools" => &ADDITIONAL_TOOLS,
        other => {
            return Err(Rejection::new(
                RejectionCode::UnsupportedInputItem,
                Some(&join(path, "type")),
                format!(
                    "Input items of type `{other}` are not supported. Accepted: message, reasoning, function_call, function_call_output, custom_tool_call, custom_tool_call_output, additional_tools."
                ),
            ));
        }
    };
    shape.apply(object, path).map(Value::Object)
}

fn message_content(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::String(_) => Ok(value),
        Value::Array(parts) => each(parts, path, |part, path| {
            content_part(
                part,
                path,
                &["input_text", "output_text", "refusal", "input_image"],
            )
        }),
        _ => Err(invalid(
            path,
            "must be a string or an array of content parts",
        )),
    }
}

fn tool_output(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::String(_) => Ok(value),
        Value::Array(parts) => each(parts, path, |part, path| {
            content_part(part, path, &["input_text", "input_image"])
        }),
        _ => Err(invalid(
            path,
            "must be a string or an array of content parts",
        )),
    }
}

fn content_part(value: Value, path: &str, accepted: &[&str]) -> Result<Value, Rejection> {
    let (kind, object) = typed_object(value, path)?;
    if !accepted.contains(&kind.as_str()) {
        return Err(Rejection::new(
            RejectionCode::UnsupportedInputItem,
            Some(&join(path, "type")),
            format!(
                "Content parts of type `{kind}` are not supported here. Accepted: {}.",
                accepted.join(", ")
            ),
        ));
    }
    let shape = match kind.as_str() {
        "input_text" => &INPUT_TEXT,
        "output_text" => &OUTPUT_TEXT,
        "refusal" => &REFUSAL,
        _ => &INPUT_IMAGE,
    };
    shape.apply(object, path).map(Value::Object)
}

fn summary_parts(value: Value, path: &str) -> Result<Value, Rejection> {
    array_of(value, path, |part, path| {
        let (kind, object) = typed_object(part, path)?;
        if kind != "summary_text" {
            return Err(invalid(&join(path, "type"), "must be `summary_text`"));
        }
        SUMMARY_TEXT.apply(object, path).map(Value::Object)
    })
}

fn reasoning_text_parts(value: Value, path: &str) -> Result<Value, Rejection> {
    array_of(value, path, |part, path| {
        let (kind, object) = typed_object(part, path)?;
        if kind != "reasoning_text" {
            return Err(invalid(&join(path, "type"), "must be `reasoning_text`"));
        }
        REASONING_TEXT.apply(object, path).map(Value::Object)
    })
}

fn tool_list(value: Value, path: &str) -> Result<Value, Rejection> {
    array_of(value, path, |tool, path| tool_definition(tool, path, false))
}

fn namespace_members(value: Value, path: &str) -> Result<Value, Rejection> {
    array_of(value, path, |tool, path| tool_definition(tool, path, true))
}

fn tool_definition(value: Value, path: &str, in_namespace: bool) -> Result<Value, Rejection> {
    let (kind, object) = typed_object(value, path)?;
    let shape = match kind.as_str() {
        "function" => &FUNCTION_TOOL,
        "custom" => &CUSTOM_TOOL,
        "namespace" if !in_namespace => &NAMESPACE_TOOL,
        "namespace" => {
            return Err(invalid(
                &join(path, "type"),
                "a namespace cannot contain another namespace",
            ));
        }
        other => return Err(hosted_tool(&join(path, "type"), other)),
    };
    shape.apply(object, path).map(Value::Object)
}

fn hosted_tool(path: &str, kind: &str) -> Rejection {
    let message = match kind {
        "web_search" | "web_search_preview" | "web_search_preview_2025_03_11" | "web_search_2025_08_26" => {
            "Hosted web search is not allowed through QuotaMiser: it is billed per call, outside the token grant. In Codex, set `web_search = \"disabled\"` in config.toml.".to_owned()
        }
        "tool_search" => {
            "`tool_search` is not allowed through QuotaMiser. Codex adds it when MCP servers or apps expose deferred tools; leave them out of the Codex profile that uses QuotaMiser.".to_owned()
        }
        other => format!(
            "Tool type `{other}` is not allowed through QuotaMiser. Accepted tool types: function, custom, and namespace containing function or custom tools."
        ),
    };
    Rejection::new(RejectionCode::HostedTool, Some(path), message)
}

fn custom_tool_format(value: Value, path: &str) -> Result<Value, Rejection> {
    let (kind, object) = typed_object(value, path)?;
    let shape = match kind.as_str() {
        "text" => &TEXT_FORMAT,
        "grammar" => &GRAMMAR_FORMAT,
        _ => return Err(invalid(&join(path, "type"), "must be `text` or `grammar`")),
    };
    shape.apply(object, path).map(Value::Object)
}

fn tool_choice(value: Value, path: &str) -> Result<Value, Rejection> {
    if let Value::String(choice) = &value {
        return match choice.as_str() {
            "auto" | "none" | "required" => Ok(value),
            _ => Err(invalid(
                path,
                "must be `auto`, `none`, `required`, or a function or custom tool",
            )),
        };
    }
    let (kind, object) = typed_object(value, path)?;
    if kind != "function" && kind != "custom" {
        return Err(invalid(
            &join(path, "type"),
            "must be `auto`, `none`, `required`, or a function or custom tool",
        ));
    }
    NAMED_TOOL_CHOICE.apply(object, path).map(Value::Object)
}

fn output_format(value: Value, path: &str) -> Result<Value, Rejection> {
    let (kind, object) = typed_object(value, path)?;
    let shape = match kind.as_str() {
        "text" | "json_object" => &PLAIN_OUTPUT_FORMAT,
        "json_schema" => &JSON_SCHEMA_FORMAT,
        _ => {
            return Err(invalid(
                &join(path, "type"),
                "must be `text`, `json_object` or `json_schema`",
            ));
        }
    };
    shape.apply(object, path).map(Value::Object)
}

fn include(value: Value, path: &str) -> Result<Value, Rejection> {
    array_of(value, path, |entry, path| match &entry {
        Value::String(name) if name == "reasoning.encrypted_content" => Ok(entry),
        _ => Err(invalid(
            path,
            "only `reasoning.encrypted_content` may be included",
        )),
    })
}

fn metadata(value: Value, path: &str) -> Result<Value, Rejection> {
    let Value::Object(entries) = &value else {
        return Err(invalid(path, "must be an object of strings"));
    };
    if let Some((key, _)) = entries.iter().find(|(_, v)| !v.is_string()) {
        return Err(invalid(&join(path, key), "must be a string"));
    }
    Ok(value)
}

fn object_of(shape: &Shape, value: Value, path: &str) -> Result<Value, Rejection> {
    let Value::Object(object) = value else {
        return Err(invalid(path, "must be an object"));
    };
    shape.apply(object, path).map(Value::Object)
}

fn typed_object(value: Value, path: &str) -> Result<(String, Map<String, Value>), Rejection> {
    let Value::Object(object) = value else {
        return Err(invalid(path, "must be an object"));
    };
    let kind = match object.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        Some(_) => return Err(invalid(&join(path, "type"), "must be a string")),
        None => return Err(missing_parameter(&join(path, "type"))),
    };
    Ok((kind, object))
}

fn array_of(
    value: Value,
    path: &str,
    check: impl Fn(Value, &str) -> Result<Value, Rejection>,
) -> Result<Value, Rejection> {
    match value {
        Value::Array(items) => each(items, path, check),
        _ => Err(invalid(path, "must be an array")),
    }
}

fn each(
    items: Vec<Value>,
    path: &str,
    check: impl Fn(Value, &str) -> Result<Value, Rejection>,
) -> Result<Value, Rejection> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| check(item, &format!("{path}[{index}]")))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn string(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::String(_) => Ok(value),
        _ => Err(invalid(path, "must be a string")),
    }
}

fn non_empty_string(value: Value, path: &str) -> Result<Value, Rejection> {
    match &value {
        Value::String(text) if !text.is_empty() => Ok(value),
        _ => Err(invalid(path, "must be a non-empty string")),
    }
}

fn boolean(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::Bool(_) => Ok(value),
        _ => Err(invalid(path, "must be a boolean")),
    }
}

fn boolean_or_null(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::Bool(_) | Value::Null => Ok(value),
        _ => Err(invalid(path, "must be a boolean or null")),
    }
}

fn number(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::Number(_) => Ok(value),
        _ => Err(invalid(path, "must be a number")),
    }
}

fn positive_integer(value: Value, path: &str) -> Result<Value, Rejection> {
    match value.as_u64() {
        Some(n) if n > 0 => Ok(value),
        _ => Err(invalid(path, "must be a positive integer")),
    }
}

fn json_object(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::Object(_) => Ok(value),
        _ => Err(invalid(path, "must be an object")),
    }
}

/// A deferred tool is only reachable through `tool_search`, which is refused.
fn not_deferred(value: Value, path: &str) -> Result<Value, Rejection> {
    match value {
        Value::Bool(false) => Ok(value),
        _ => Err(invalid(
            path,
            "deferred tools need `tool_search`, which QuotaMiser does not allow",
        )),
    }
}

fn inline_image_url(value: Value, path: &str) -> Result<Value, Rejection> {
    match &value {
        Value::String(url) if url.starts_with("data:") => Ok(value),
        Value::String(_) => Err(Rejection::new(
            RejectionCode::RemoteReference,
            Some(path),
            "Only inline `data:` image URLs are accepted: a remote image could change between counting and generation.",
        )),
        _ => Err(invalid(path, "must be a string")),
    }
}

fn one_of(value: Value, path: &str, accepted: &[&str]) -> Result<Value, Rejection> {
    match &value {
        Value::String(text) if accepted.contains(&text.as_str()) => Ok(value),
        _ => Err(invalid(
            path,
            &format!("must be one of: {}", accepted.join(", ")),
        )),
    }
}

fn message_role(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["user", "assistant", "system", "developer"])
}

fn developer_role(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["developer"])
}

fn message_phase(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["commentary", "final_answer"])
}

fn image_detail(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["auto", "low", "high", "original"])
}

fn grammar_syntax(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["lark", "regex"])
}

/// Only efforts whose cost is output tokens within the output bound. Newer
/// levels are refused until measured.
fn reasoning_effort(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(
        value,
        path,
        &["none", "minimal", "low", "medium", "high", "xhigh"],
    )
}

fn reasoning_summary(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["auto", "concise", "detailed", "none"])
}

fn reasoning_context(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["auto", "current_turn", "all_turns"])
}

fn verbosity(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["low", "medium", "high"])
}

fn cache_ttl(value: Value, path: &str) -> Result<Value, Rejection> {
    one_of(value, path, &["30m"])
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn invalid(path: &str, what: &str) -> Rejection {
    Rejection::new(
        RejectionCode::InvalidValue,
        Some(path),
        format!("`{path}` {what}."),
    )
}

fn missing_parameter(path: &str) -> Rejection {
    Rejection::new(
        RejectionCode::MissingParameter,
        Some(path),
        format!("`{path}` is required."),
    )
}

fn unsupported_parameter(path: &str) -> Rejection {
    let mut message =
        format!("`{path}` is not supported by QuotaMiser (allowlist {ALLOWLIST_VERSION}).");
    if path == "stream_options" {
        message.push(' ');
        message.push_str(CODEX_PROVIDER_HINT);
    }
    Rejection::new(RejectionCode::UnsupportedParameter, Some(path), message)
}

fn unsupported_field(path: &str, what: &str, key: &str) -> Rejection {
    let mut message =
        format!("`{key}` is not supported in a {what} (allowlist {ALLOWLIST_VERSION}).");
    if matches!(
        key,
        "internal_chat_message_metadata_passthrough" | "encrypted_function_args"
    ) {
        message.push(' ');
        message.push_str(CODEX_PROVIDER_HINT);
    }
    Rejection::new(RejectionCode::UnsupportedParameter, Some(path), message)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn parse(value: Value) -> Result<CreateRequest, Rejection> {
        CreateRequest::parse(&serde_json::to_vec(&value).unwrap())
    }

    fn upstream(request: &CreateRequest) -> Value {
        serde_json::from_slice(&request.openai_body()).unwrap()
    }

    fn rejected(value: Value) -> Rejection {
        parse(value).expect_err("should be refused")
    }

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    fn function_tool() -> Value {
        json!({
            "type": "function",
            "name": "exec_command",
            "description": "Runs a command.",
            "strict": false,
            "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}, "workdir": {"type": "string"}}, "required": ["cmd"]}
        })
    }

    fn custom_tool() -> Value {
        json!({
            "type": "custom",
            "name": "apply_patch",
            "description": "Applies a patch.",
            "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}
        })
    }

    /// The shape Codex 0.154.0 sends for a GPT-5.6 model, mid-conversation.
    fn codex_lite() -> Value {
        json!({
            "model": "gpt-5.6-terra",
            "instructions": "",
            "input": [
                {"type": "additional_tools", "id": "at_1", "role": "developer",
                 "tools": [{"type": "namespace", "name": "functions", "description": "", "tools": [function_tool(), custom_tool()]}]},
                {"type": "message", "id": "msg_1", "role": "developer", "content": [{"type": "input_text", "text": "You are Codex."}]},
                user("List the files."),
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Listing."}], "encrypted_content": "gAAAA"},
                {"type": "function_call", "id": "fc_1", "name": "exec_command", "namespace": "functions", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_1"},
                {"type": "function_call_output", "call_id": "call_1", "output": "Cargo.toml\nsrc"},
                {"type": "custom_tool_call", "id": "ctc_1", "status": "completed", "call_id": "call_2", "name": "apply_patch", "input": "*** Begin Patch"},
                {"type": "custom_tool_call_output", "call_id": "call_2", "output": [{"type": "input_text", "text": "Done."}]},
                {"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Two files.", "annotations": []}]}
            ],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {"effort": "medium", "summary": "auto", "context": "all_turns"},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "service_tier": "priority",
            "prompt_cache_key": "019a-session",
            "text": {"verbosity": "low"},
            "client_metadata": {"x-codex-git-remote": "git@github.com:example/private.git"}
        })
    }

    /// The shape OpenClaw's Responses transport sends.
    fn openclaw() -> Value {
        json!({
            "model": "gpt-5.6-terra",
            "input": [
                {"role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "message", "role": "assistant", "id": "msg_9", "status": "completed", "content": [{"type": "output_text", "text": "Hello"}]},
                user("What's in this picture?"),
                {"type": "message", "role": "user", "content": [{"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo=", "detail": "low"}]}
            ],
            "stream": true,
            "prompt_cache_key": "agent-main",
            "prompt_cache_options": {"ttl": "30m"},
            "instructions": "You are a helpful agent.",
            "metadata": {"session": "s-1"},
            "max_output_tokens": 4096,
            "temperature": 0.7,
            "tools": [function_tool()],
            "tool_choice": "auto",
            "reasoning": {"effort": "low", "summary": "auto"},
            "include": ["reasoning.encrypted_content"]
        })
    }

    fn any_ids(value: &Value) -> bool {
        value["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.get("id").is_some() || item.get("status").is_some())
    }

    #[test]
    fn a_codex_lite_request_is_accepted_and_normalized() {
        let request = parse(codex_lite()).unwrap();
        assert_eq!(request.model(), "gpt-5.6-terra");
        assert_eq!(request.max_output_tokens(), None);
        assert_eq!(request.flat_text(), None);

        let body = upstream(&request);
        assert!(
            !any_ids(&body),
            "history travels by content, not by stored ids"
        );
        assert!(body.get("client_metadata").is_none());
        assert_eq!(body["store"], true);
        assert_eq!(body["background"], true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["service_tier"], "default");
        assert_eq!(
            body["input"][8]["content"][0],
            json!({"type": "output_text", "text": "Two files."})
        );
        assert_eq!(
            body["input"][0]["tools"][0]["tools"][1]["name"],
            "apply_patch"
        );
        assert_eq!(body["reasoning"]["context"], "all_turns");
    }

    #[test]
    fn an_openclaw_request_is_accepted_and_normalized() {
        let request = parse(openclaw()).unwrap();
        assert_eq!(request.max_output_tokens(), Some(4096));
        let body = upstream(&request);
        assert!(!any_ids(&body));
        assert!(
            body.get("prompt_cache_options").is_none(),
            "the default TTL is dropped"
        );
        assert_eq!(body["metadata"]["session"], "s-1");
        assert_eq!(
            body["input"][0],
            json!({"role": "user", "content": [{"type": "input_text", "text": "Hi"}]})
        );
    }

    #[test]
    fn field_order_is_kept_and_the_fixed_fields_come_last() {
        let body = upstream(&parse(codex_lite()).unwrap());
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "model",
                "instructions",
                "input",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "include",
                "prompt_cache_key",
                "text",
                "background",
                "store",
                "stream",
                "service_tier"
            ]
        );
        let properties = &body["input"][0]["tools"][0]["tools"][0]["parameters"]["properties"];
        let order: Vec<&str> = properties
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            order,
            ["cmd", "workdir"],
            "schema property order is generation order"
        );
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        for original in [codex_lite(), openclaw()] {
            let once = upstream(&parse(original).unwrap());
            // Clients may not ask for background mode; everything else in the
            // upstream body is itself an acceptable client body.
            let mut resent = once.clone();
            resent.as_object_mut().unwrap().shift_remove("background");
            let again = upstream(&parse(resent).unwrap());
            assert_eq!(once, again);
        }
    }

    #[test]
    fn duplicate_keys_are_refused_at_any_depth() {
        for body in [
            r#"{"model":"a","model":"b","input":"x","stream":true}"#,
            r#"{"model":"a","input":"x","stream":true,"tools":[{"type":"function","name":"f","parameters":{"type":"object","type":"string"}}]}"#,
        ] {
            let rejection = CreateRequest::parse(body.as_bytes()).unwrap_err();
            assert_eq!(rejection.code, RejectionCode::DuplicateKey, "{body}");
        }
    }

    #[test]
    fn malformed_json_is_refused() {
        for body in [&b"{"[..], b"[]", b"{\"model\":\"a\"} trailing", b"\xff"] {
            let rejection = CreateRequest::parse(body).unwrap_err();
            assert_eq!(rejection.code, RejectionCode::InvalidJson, "{body:?}");
        }
    }

    #[test]
    fn unlisted_fields_are_refused_wherever_they_appear() {
        let cases = [
            ("truncation", json!({"truncation": "auto"})),
            ("user", json!({"user": "u"})),
            ("max_tool_calls", json!({"max_tool_calls": 3})),
            (
                "prompt_cache_retention",
                json!({"prompt_cache_retention": "24h"}),
            ),
            (
                "reasoning.mode",
                json!({"reasoning": {"effort": "low", "mode": "pro"}}),
            ),
            (
                "text.format.extra",
                json!({"text": {"format": {"type": "text", "extra": 1}}}),
            ),
            (
                "input[0].extra",
                json!({"input": [{"type": "message", "role": "user", "content": "hi", "extra": 1}]}),
            ),
            (
                "tools[0].extra",
                json!({"tools": [{"type": "function", "name": "f", "extra": 1}]}),
            ),
            (
                "input[0].content[0].file_id",
                json!({"input": [{"role": "user", "content": [{"type": "input_image", "file_id": "file-1"}]}]}),
            ),
        ];
        for (param, extra) in cases {
            let mut body = json!({"model": "gpt-5.6-terra", "input": "hi", "stream": true});
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            let rejection = rejected(body);
            assert_eq!(
                rejection.code,
                RejectionCode::UnsupportedParameter,
                "{param}"
            );
            assert_eq!(rejection.param.as_deref(), Some(param));
        }
    }

    #[test]
    fn codex_builtin_provider_fields_point_at_the_fix() {
        let mut body = codex_lite();
        body["stream_options"] = json!({"reasoning_summary_delivery": "sequential_cutoff"});
        assert!(rejected(body).message.contains("custom model provider"));

        let mut body = codex_lite();
        body["input"][2]["internal_chat_message_metadata_passthrough"] = json!({});
        let rejection = rejected(body);
        assert_eq!(
            rejection.param.as_deref(),
            Some("input[2].internal_chat_message_metadata_passthrough")
        );
        assert!(rejection.message.contains("custom model provider"));
    }

    #[test]
    fn server_side_state_is_refused_but_null_is_absent() {
        for (field, value) in [
            ("previous_response_id", json!("resp_1")),
            ("conversation", json!("conv_1")),
            ("prompt", json!({"id": "pmpt_1"})),
        ] {
            let mut body = json!({"model": "m", "input": "hi", "stream": true});
            body[field] = value;
            assert_eq!(
                rejected(body.clone()).code,
                RejectionCode::ServerSideState,
                "{field}"
            );
            body[field] = Value::Null;
            assert!(
                upstream(&parse(body).unwrap()).get(field).is_none(),
                "{field}"
            );
        }
    }

    #[test]
    fn hosted_tools_are_refused_with_guidance() {
        let cases = [
            (
                json!({"type": "web_search", "external_web_access": false}),
                "web_search = \"disabled\"",
            ),
            (
                json!({"type": "tool_search", "execution": "client"}),
                "MCP servers",
            ),
            (json!({"type": "image_generation"}), "not allowed"),
            (
                json!({"type": "file_search", "vector_store_ids": ["vs_1"]}),
                "not allowed",
            ),
            (json!({"type": "local_shell"}), "not allowed"),
            (json!({"type": "mcp", "server_label": "x"}), "not allowed"),
        ];
        for (tool, hint) in cases {
            let body =
                json!({"model": "m", "input": "hi", "stream": true, "tools": [tool.clone()]});
            let rejection = rejected(body);
            assert_eq!(rejection.code, RejectionCode::HostedTool, "{tool}");
            assert_eq!(rejection.param.as_deref(), Some("tools[0].type"));
            assert!(rejection.message.contains(hint), "{}", rejection.message);
        }

        let mut lite = codex_lite();
        lite["input"][0]["tools"][0]["tools"][0] = json!({"type": "web_search"});
        assert_eq!(
            rejected(lite).param.as_deref(),
            Some("input[0].tools[0].tools[0].type")
        );
    }

    #[test]
    fn namespaces_hold_only_function_and_custom_tools() {
        let nested = json!({"type": "namespace", "name": "outer", "tools": [{"type": "namespace", "name": "inner", "tools": []}]});
        let body = json!({"model": "m", "input": "hi", "stream": true, "tools": [nested]});
        assert_eq!(rejected(body).code, RejectionCode::InvalidValue);

        let deferred = json!({"type": "function", "name": "f", "defer_loading": true});
        let body = json!({"model": "m", "input": "hi", "stream": true, "tools": [deferred]});
        assert_eq!(
            rejected(body).param.as_deref(),
            Some("tools[0].defer_loading")
        );
    }

    #[test]
    fn history_from_unaccepted_tools_is_refused() {
        for kind in [
            "web_search_call",
            "local_shell_call",
            "tool_search_call",
            "tool_search_output",
            "image_generation_call",
            "compaction",
            "context_compaction",
            "item_reference",
            "agent_message",
            "file_search_call",
            "computer_call",
            "mcp_call",
        ] {
            let body = json!({"model": "m", "stream": true, "input": [user("hi"), {"type": kind, "id": "x"}]});
            let rejection = rejected(body);
            assert_eq!(
                rejection.code,
                RejectionCode::UnsupportedInputItem,
                "{kind}"
            );
            assert_eq!(rejection.param.as_deref(), Some("input[1].type"));
        }
    }

    #[test]
    fn a_reasoning_item_must_carry_its_encrypted_content() {
        let body = json!({"model": "m", "stream": true, "input": [{"type": "reasoning", "id": "rs_1", "summary": []}]});
        let rejection = rejected(body);
        assert_eq!(rejection.code, RejectionCode::MissingParameter);
        assert_eq!(
            rejection.param.as_deref(),
            Some("input[0].encrypted_content")
        );
    }

    #[test]
    fn only_inline_images_are_accepted() {
        let image = |url: &str| json!({"model": "m", "stream": true, "input": [{"role": "user", "content": [{"type": "input_image", "image_url": url}]}]});
        assert!(parse(image("data:image/png;base64,AAAA")).is_ok());
        for url in [
            "https://example.com/a.png",
            "http://127.0.0.1/a.png",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                rejected(image(url)).code,
                RejectionCode::RemoteReference,
                "{url}"
            );
        }
        let file = json!({"model": "m", "stream": true, "input": [{"role": "user", "content": [{"type": "input_file", "file_url": "https://example.com/a.pdf"}]}]});
        assert_eq!(rejected(file).code, RejectionCode::UnsupportedInputItem);
    }

    #[test]
    fn streaming_is_required() {
        let missing = rejected(json!({"model": "m", "input": "hi"}));
        assert_eq!(missing.code, RejectionCode::MissingParameter);
        let off = rejected(json!({"model": "m", "input": "hi", "stream": false}));
        assert_eq!(off.param.as_deref(), Some("stream"));
    }

    #[test]
    fn model_and_input_are_required() {
        assert_eq!(
            rejected(json!({"input": "hi", "stream": true}))
                .param
                .as_deref(),
            Some("model")
        );
        assert_eq!(
            rejected(json!({"model": "m", "stream": true}))
                .param
                .as_deref(),
            Some("input")
        );
        assert_eq!(
            rejected(json!({"model": "", "input": "hi", "stream": true}))
                .param
                .as_deref(),
            Some("model")
        );
    }

    #[test]
    fn client_background_mode_is_refused_but_false_is_absent() {
        assert_eq!(
            rejected(json!({"model": "m", "input": "hi", "stream": true, "background": true}))
                .param
                .as_deref(),
            Some("background")
        );
        let body = upstream(
            &parse(json!({"model": "m", "input": "hi", "stream": true, "background": false}))
                .unwrap(),
        );
        assert_eq!(body["background"], true);
    }

    #[test]
    fn enumerated_values_outside_the_list_are_refused() {
        let cases = [
            (
                "reasoning.effort",
                json!({"reasoning": {"effort": "ultra"}}),
            ),
            (
                "reasoning.context",
                json!({"reasoning": {"context": "everything"}}),
            ),
            ("text.verbosity", json!({"text": {"verbosity": "max"}})),
            (
                "include[1]",
                json!({"include": ["reasoning.encrypted_content", "web_search_call.action.sources"]}),
            ),
            (
                "tool_choice.type",
                json!({"tool_choice": {"type": "web_search_preview"}}),
            ),
            (
                "prompt_cache_options.ttl",
                json!({"prompt_cache_options": {"ttl": "24h"}}),
            ),
            ("max_output_tokens", json!({"max_output_tokens": 0})),
            ("max_output_tokens", json!({"max_output_tokens": 1.5})),
        ];
        for (param, extra) in cases {
            let mut body = json!({"model": "m", "input": "hi", "stream": true});
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            let rejection = rejected(body);
            assert_eq!(rejection.code, RejectionCode::InvalidValue, "{param}");
            assert_eq!(rejection.param.as_deref(), Some(param));
        }
    }

    #[test]
    fn the_flat_form_excludes_anything_that_adds_input() {
        let flat =
            parse(json!({"model": "m", "input": "hi", "instructions": "be brief", "stream": true}))
                .unwrap();
        assert_eq!(
            flat.flat_text(),
            Some(FlatText {
                instructions: Some("be brief"),
                input: "hi"
            })
        );

        for extra in [
            json!({"tools": [function_tool()]}),
            json!({"tool_choice": "none"}),
            json!({"text": {"format": {"type": "json_schema", "name": "a", "schema": {"type": "object"}}}}),
        ] {
            let mut body = json!({"model": "m", "input": "hi", "stream": true});
            for (key, value) in extra.as_object().unwrap() {
                body[key] = value.clone();
            }
            assert_eq!(parse(body).unwrap().flat_text(), None, "{extra}");
        }
        assert_eq!(
            parse(json!({"model": "m", "input": [user("hi")], "stream": true}))
                .unwrap()
                .flat_text(),
            None
        );
        let plain_format = parse(json!({"model": "m", "input": "hi", "stream": true, "text": {"format": {"type": "text"}}})).unwrap();
        assert!(plain_format.flat_text().is_some());
    }

    #[test]
    fn the_count_bodies_cover_additional_tools_a_second_time() {
        let request = parse(codex_lite()).unwrap();
        let bodies = request.input_count_bodies();
        assert_eq!(bodies.len(), 2);

        let whole = &bodies[0];
        let keys: Vec<&str> = whole
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "model",
                "instructions",
                "input",
                "tool_choice",
                "parallel_tool_calls",
                "reasoning",
                "text"
            ]
        );
        assert_eq!(whole["input"], upstream(&request)["input"]);

        let tools = &bodies[1];
        assert_eq!(tools["tools"], upstream(&request)["input"][0]["tools"]);
        let items = tools["input"].as_array().unwrap();
        assert_eq!(items.len(), 8);
        assert!(items.iter().all(|item| item["type"] != "additional_tools"));
    }

    #[test]
    fn a_count_body_for_hoisted_tools_drops_a_named_tool_choice() {
        let mut lite = codex_lite();
        lite["tool_choice"] = json!({"type": "function", "name": "exec_command"});
        lite["input"].as_array_mut().unwrap().push(
            json!({"type": "additional_tools", "role": "developer", "tools": [custom_tool()]}),
        );
        let bodies = parse(lite).unwrap().input_count_bodies();
        assert_eq!(bodies.len(), 3, "one extra body per additional_tools item");
        assert!(bodies[0].get("tool_choice").is_some());
        assert!(bodies[1].get("tool_choice").is_none());
        assert_eq!(bodies[2]["tools"], json!([custom_tool()]));
    }

    #[test]
    fn an_output_schema_is_part_of_the_counted_input() {
        let format = json!({"type": "json_schema", "name": "answer", "schema": {"type": "object"}});
        let request = parse(json!({"model": "m", "input": "hi", "stream": true, "text": {"format": format.clone()}})).unwrap();
        assert_eq!(request.input_count_bodies()[0]["text"]["format"], format);
    }

    #[test]
    fn a_request_without_additional_tools_is_counted_once() {
        let bodies = parse(openclaw()).unwrap().input_count_bodies();
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].get("max_output_tokens").is_none());
        assert!(bodies[0].get("stream").is_none());
    }

    #[test]
    fn the_openrouter_body_leaves_out_what_openai_alone_needs() {
        let request = parse(codex_lite()).unwrap();
        let body: Value =
            serde_json::from_slice(&request.openrouter_body("nex-agi/nex-n2.5-pro:free")).unwrap();

        assert_eq!(body["model"], "nex-agi/nex-n2.5-pro:free");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["store"], false,
            "OpenRouter refuses store: true outright"
        );
        assert!(body.get("background").is_none());
        assert!(body.get("service_tier").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("include").is_none());
        // Only the measured part of each OpenAI-shaped knob travels.
        assert_eq!(body["reasoning"], json!({"effort": "medium"}));
        assert!(
            body.get("text").is_none(),
            "verbosity alone does not travel"
        );
        assert_eq!(body["input"], upstream(&request)["input"]);
    }

    #[test]
    fn an_output_schema_travels_to_openrouter() {
        let format = json!({"type": "json_schema", "name": "answer", "schema": {"type": "object"}});
        let request = parse(json!({
            "model": "gpt-5.6-terra", "input": "hi", "stream": true,
            "text": {"verbosity": "low", "format": format.clone()}
        }))
        .unwrap();
        let body: Value = serde_json::from_slice(&request.openrouter_body("x/y:free")).unwrap();
        assert_eq!(body["text"], json!({"format": format}));
    }

    #[test]
    fn tool_usage_finds_every_shape_wherever_it_is_declared() {
        assert_eq!(
            parse(codex_lite()).unwrap().tool_usage(),
            ToolUsage {
                function: true,
                custom: true,
                namespace: true,
                additional_tools: true
            },
            "Codex declares a namespace of function and custom tools in an additional_tools item"
        );

        assert_eq!(
            parse(openclaw()).unwrap().tool_usage(),
            ToolUsage {
                function: true,
                ..ToolUsage::default()
            }
        );

        let plain = parse(json!({"model": "m", "input": "hi", "stream": true})).unwrap();
        assert_eq!(plain.tool_usage(), ToolUsage::default());

        let top_level_custom = parse(json!({
            "model": "m", "input": "hi", "stream": true, "tools": [custom_tool()]
        }))
        .unwrap();
        assert_eq!(
            top_level_custom.tool_usage(),
            ToolUsage {
                custom: true,
                ..ToolUsage::default()
            }
        );
    }

    #[test]
    fn a_structured_output_is_visible_to_the_router() {
        let plain = parse(json!({"model": "m", "input": "hi", "stream": true})).unwrap();
        assert!(!plain.needs_structured_output());

        let text_format = parse(json!({
            "model": "m", "input": "hi", "stream": true, "text": {"format": {"type": "text"}}
        }))
        .unwrap();
        assert!(!text_format.needs_structured_output());

        let schema = parse(json!({
            "model": "m", "input": "hi", "stream": true,
            "text": {"format": {"type": "json_schema", "name": "a", "schema": {"type": "object"}}}
        }))
        .unwrap();
        assert!(schema.needs_structured_output());
    }

    #[test]
    fn a_rejection_becomes_an_openai_error_body() {
        let rejection = rejected(
            json!({"model": "m", "input": "hi", "stream": true, "tools": [{"type": "web_search"}]}),
        );
        let body = rejection.error_body();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "hosted_tool_unsupported");
        assert_eq!(body["error"]["param"], "tools[0].type");
    }

    const TOP_LEVEL: [&str; 23] = [
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "text",
        "include",
        "max_output_tokens",
        "temperature",
        "top_p",
        "metadata",
        "prompt_cache_key",
        "prompt_cache_options",
        "client_metadata",
        "store",
        "service_tier",
        "stream",
        "background",
        "previous_response_id",
        "conversation",
        "prompt",
    ];

    proptest! {
        /// Deny by default: a top-level name outside the list is refused,
        /// whatever its value.
        #[test]
        fn unknown_top_level_fields_are_refused(name in "[a-z_]{1,24}", value in any::<i64>()) {
            prop_assume!(!TOP_LEVEL.contains(&name.as_str()));
            let mut body = json!({"model": "m", "input": "hi", "stream": true});
            body[name.as_str()] = json!(value);
            let rejection = parse(body).unwrap_err();
            prop_assert_eq!(rejection.code, RejectionCode::UnsupportedParameter);
            prop_assert_eq!(rejection.param, Some(name));
        }

        /// The same for fields of an input message.
        #[test]
        fn unknown_message_fields_are_refused(name in "[a-z_]{1,24}") {
            prop_assume!(!["type", "id", "status", "role", "content", "phase"].contains(&name.as_str()));
            let mut message = user("hi");
            message[name.as_str()] = json!(1);
            let body = json!({"model": "m", "stream": true, "input": [message]});
            let rejection = parse(body).unwrap_err();
            prop_assert_eq!(rejection.code, RejectionCode::UnsupportedParameter);
        }
    }
}
