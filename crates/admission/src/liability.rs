//! The liability of a request: the most it can consume.
//!
//! The ledger reserves exactly this number, so it must be an upper bound and
//! never an estimate. It is the sum of an input bound and an output bound.
//!
//! The input bound has a fast path for a flat text request whose model uses
//! a byte-level encoding covering all 256 byte values. Merges only ever reduce
//! the token count, so the text's tokens never exceed its UTF-8 byte length,
//! whatever the vocabulary. Request framing (role and boundary tokens) is not
//! in the text; it is covered by a per-node allowance. Everything else goes to
//! the provider's count instead. That count is exact for most request shapes
//! but not all: tools declared in an `additional_tools` input item are counted
//! once less than generation renders them, so the count to use is the sum over
//! every body `CreateRequest::input_count_bodies` returns (ADR-0008).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// Includes reasoning tokens.
    pub max_output_tokens: u64,
    /// Only when the model is known to use a byte-level encoding covering all
    /// byte values. Unpublished encodings, such as GPT-5.6's, must be `false`.
    pub byte_level_encoding_known: bool,
}

/// The request's input, reduced to what bounding needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputShape<'a> {
    /// A single text input and at most a single text instructions, with no
    /// content parts, messages, tools or attachments.
    FlatText {
        instructions: Option<&'a str>,
        input: &'a str,
    },
    /// Anything with structure. Framing grows per part in ways the byte
    /// bound does not cover, so these always use the provider's count.
    Structured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EstimatorPolicy {
    framing_allowance_per_node: u64,
}

/// Placeholder until measured against the provider's token count. The one
/// observation so far, a single seven-byte input counted at 9 tokens, puts
/// its framing between 2 and 8 tokens.
pub const DEFAULT_FRAMING_ALLOWANCE_PER_NODE: u64 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EstimateError {
    #[error("the framing allowance must be positive")]
    NoFramingAllowance,
    #[error("an output cap of zero admits no response")]
    ZeroOutputCap,
    #[error("liability overflowed")]
    Overflow,
}

impl EstimatorPolicy {
    pub fn new(framing_allowance_per_node: u64) -> Result<Self, EstimateError> {
        if framing_allowance_per_node == 0 {
            return Err(EstimateError::NoFramingAllowance);
        }
        Ok(Self {
            framing_allowance_per_node,
        })
    }
}

impl Default for EstimatorPolicy {
    fn default() -> Self {
        Self {
            framing_allowance_per_node: DEFAULT_FRAMING_ALLOWANCE_PER_NODE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liability {
    Known(u64),
    /// Obtain the provider's input count, summed over the count bodies, then
    /// call [`with_exact_input`].
    NeedsExactInput {
        output_bound: u64,
    },
}

/// The output bound: the client's cap when given, never above the model's
/// maximum, which applies when the client gives none.
pub fn output_bound(spec: &ModelSpec, requested_cap: Option<u64>) -> Result<u64, EstimateError> {
    match requested_cap {
        Some(0) => Err(EstimateError::ZeroOutputCap),
        Some(cap) => Ok(cap.min(spec.max_output_tokens)),
        None => Ok(spec.max_output_tokens),
    }
}

/// The structural input bound, or `None` when only an exact count will do.
pub fn structural_input_bound(
    policy: &EstimatorPolicy,
    spec: &ModelSpec,
    shape: InputShape<'_>,
) -> Result<Option<u64>, EstimateError> {
    let InputShape::FlatText {
        instructions,
        input,
    } = shape
    else {
        return Ok(None);
    };
    if !spec.byte_level_encoding_known {
        return Ok(None);
    }
    // Every text node the provider will receive, each counted in bytes.
    let nodes: Vec<&str> = instructions
        .into_iter()
        .chain(std::iter::once(input))
        .collect();
    let text_bytes = nodes
        .iter()
        .try_fold(0u64, |sum, text| sum.checked_add(text.len() as u64))
        .ok_or(EstimateError::Overflow)?;
    let framing = policy
        .framing_allowance_per_node
        .checked_mul(nodes.len() as u64)
        .ok_or(EstimateError::Overflow)?;
    text_bytes
        .checked_add(framing)
        .map(Some)
        .ok_or(EstimateError::Overflow)
}

pub fn estimate(
    policy: &EstimatorPolicy,
    spec: &ModelSpec,
    shape: InputShape<'_>,
    requested_cap: Option<u64>,
) -> Result<Liability, EstimateError> {
    let output_bound = output_bound(spec, requested_cap)?;
    match structural_input_bound(policy, spec, shape)? {
        Some(input) => input
            .checked_add(output_bound)
            .map(Liability::Known)
            .ok_or(EstimateError::Overflow),
        None => Ok(Liability::NeedsExactInput { output_bound }),
    }
}

/// Completes a liability once the provider has counted the input: the sum of
/// its counts over the count bodies, never a single count of the request.
pub fn with_exact_input(exact_input_tokens: u64, output_bound: u64) -> Result<u64, EstimateError> {
    exact_input_tokens
        .checked_add(output_bound)
        .ok_or(EstimateError::Overflow)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const KNOWN: ModelSpec = ModelSpec {
        max_output_tokens: 128_000,
        byte_level_encoding_known: true,
    };
    const UNKNOWN: ModelSpec = ModelSpec {
        max_output_tokens: 128_000,
        byte_level_encoding_known: false,
    };

    fn policy() -> EstimatorPolicy {
        EstimatorPolicy::new(64).unwrap()
    }

    #[test]
    fn flat_text_is_bytes_plus_framing_plus_output() {
        let shape = InputShape::FlatText {
            instructions: None,
            input: "Say OK.",
        };
        assert_eq!(
            estimate(&policy(), &KNOWN, shape, Some(16)),
            Ok(Liability::Known(7 + 64 + 16))
        );
    }

    #[test]
    fn instructions_are_counted_as_a_second_node() {
        let without = InputShape::FlatText {
            instructions: None,
            input: "hello",
        };
        let with = InputShape::FlatText {
            instructions: Some("be brief"),
            input: "hello",
        };
        let a = structural_input_bound(&policy(), &KNOWN, without)
            .unwrap()
            .unwrap();
        let b = structural_input_bound(&policy(), &KNOWN, with)
            .unwrap()
            .unwrap();
        assert_eq!(b - a, "be brief".len() as u64 + 64);
    }

    #[test]
    fn bytes_not_characters() {
        let shape = InputShape::FlatText {
            instructions: None,
            input: "日本語",
        };
        assert_eq!(
            structural_input_bound(&policy(), &KNOWN, shape),
            Ok(Some(9 + 64))
        );
    }

    #[test]
    fn structure_or_an_unknown_encoding_needs_an_exact_count() {
        assert_eq!(
            estimate(&policy(), &KNOWN, InputShape::Structured, None),
            Ok(Liability::NeedsExactInput {
                output_bound: 128_000
            })
        );
        let flat = InputShape::FlatText {
            instructions: None,
            input: "hi",
        };
        assert_eq!(
            estimate(&policy(), &UNKNOWN, flat, Some(10)),
            Ok(Liability::NeedsExactInput { output_bound: 10 })
        );
    }

    #[test]
    fn output_bound_follows_the_cap_but_never_exceeds_the_model() {
        assert_eq!(output_bound(&KNOWN, None), Ok(128_000));
        assert_eq!(output_bound(&KNOWN, Some(4_000)), Ok(4_000));
        assert_eq!(output_bound(&KNOWN, Some(1_000_000)), Ok(128_000));
        assert_eq!(
            output_bound(&KNOWN, Some(0)),
            Err(EstimateError::ZeroOutputCap)
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        assert_eq!(with_exact_input(u64::MAX, 1), Err(EstimateError::Overflow));
        assert_eq!(
            EstimatorPolicy::new(0),
            Err(EstimateError::NoFramingAllowance)
        );
    }

    proptest! {
        /// Against the worst a byte-level tokenizer can do — one token per
        /// byte — plus at most the allowance of framing per node, the fast
        /// path bound always holds. Counting characters instead of bytes, or
        /// dropping the instructions, breaks this.
        #[test]
        fn fast_path_dominates_worst_case_byte_level_tokenization(
            instructions in proptest::option::of(any::<String>()),
            input in any::<String>(),
            framing_per_node in 0u64..=64,
            cap in proptest::option::of(1u64..200_000),
        ) {
            let shape = InputShape::FlatText { instructions: instructions.as_deref(), input: &input };
            let Liability::Known(liability) = estimate(&policy(), &KNOWN, shape, cap).unwrap() else {
                panic!("a flat text request on a known encoding must take the fast path");
            };
            let nodes = 1 + u64::from(instructions.is_some());
            let worst_input = instructions.as_deref().map_or(0, |s| s.len() as u64)
                + input.len() as u64
                + framing_per_node * nodes;
            let worst_output = cap.unwrap_or(KNOWN.max_output_tokens).min(KNOWN.max_output_tokens);
            prop_assert!(liability >= worst_input + worst_output);
        }
    }
}
