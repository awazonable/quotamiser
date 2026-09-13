//! The configuration file, and the typed pieces the runtime builds from it.
//!
//! Credentials are never in the file: it names the environment variables that
//! carry them. Unknown keys are refused, like everything else here, so a
//! misspelled setting stops startup instead of silently taking its default.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use quotamiser_admission::epoch::{ClockPolicy, PolicyError};
use quotamiser_admission::liability::ModelSpec;
use quotamiser_ledger::{LedgerConfig, RequestWindow};
use serde::Deserialize;

use crate::upstream::UpstreamConfig;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("the environment variable {0} is not set, or is empty")]
    MissingCredential(String),
    #[error("`bind` is not an address: {0}")]
    BadBindAddress(String),
    #[error(
        "`bind` must be a loopback address: QuotaMiser has no authentication, so it must not be reachable from the network"
    )]
    NotLoopback,
    #[error("at least one pool and one model must be configured")]
    Empty,
    #[error("model {model} names pool {pool}, which is not configured")]
    UnknownPool { model: String, pool: String },
    #[error("model {0} has a maximum output of zero tokens")]
    ZeroOutput(String),
    #[error("pool {0} is configured twice")]
    DuplicatePool(String),
    #[error("model {0} is configured twice")]
    DuplicateModel(String),
    #[error(
        "pool {0} is reserved for the request counter, which counts requests rather than tokens"
    )]
    PoolNameReserved(String),
    #[error("the OpenRouter route is enabled but lists no models, so it could never serve one")]
    NoOpenRouterModels,
    #[error("the OpenRouter rate-limit window must allow at least one request in a positive time")]
    EmptyOpenRouterWindow,
    #[error("the clock policy is not acceptable: {0}")]
    Clock(#[from] PolicyError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub upstream: Upstream,
    pub ledger: Ledger,
    #[serde(default)]
    pub clock: Clock,
    #[serde(default)]
    pub safety: Safety,
    /// The second free route. Absent means OpenAI only.
    #[serde(default)]
    pub openrouter: Option<OpenRouterSection>,
    #[serde(rename = "pool")]
    pub pools: Vec<Pool>,
    #[serde(rename = "model")]
    pub models: Vec<Model>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Loopback only. There is no authentication.
    pub bind: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Including the version prefix, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// The environment variable holding the inference key.
    pub api_key_env: String,
    /// The environment variable holding the Admin key, used only to read the
    /// Usage API.
    pub admin_key_env: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default = "ten")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "one_twenty")]
    pub read_timeout_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    pub path: PathBuf,
    /// Must be on a different volume from `path`, so that losing one does not
    /// lose both.
    pub external_record_path: PathBuf,
    pub lock_dir: PathBuf,
    /// The organization whose grant this ledger is the inventory for.
    pub organization: String,
    /// Only for a single-volume test machine. Never in real use.
    #[serde(default)]
    pub allow_same_volume_external_record: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    #[serde(default = "five")]
    pub reading_uncertainty_seconds: i64,
    #[serde(default = "three_hundred")]
    pub open_delay_seconds: i64,
    #[serde(default = "nine_hundred")]
    pub max_reading_age_seconds: i64,
    /// How often the provider's clock is re-read. Free of charge.
    #[serde(default = "one_twenty")]
    pub refresh_seconds: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            reading_uncertainty_seconds: five(),
            open_delay_seconds: three_hundred(),
            max_reading_age_seconds: nine_hundred(),
            refresh_seconds: one_twenty(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Safety {
    /// How long a data-sharing check stands before admission stops.
    #[serde(default = "nine_hundred_u64")]
    pub data_sharing_ttl_seconds: u64,
    /// The liability a single verification may cover, as a fraction of the
    /// day's grant: 4 means a quarter of it.
    #[serde(default = "four")]
    pub liability_budget_divisor: u64,
}

impl Default for Safety {
    fn default() -> Self {
        Self {
            data_sharing_ttl_seconds: nine_hundred_u64(),
            liability_budget_divisor: four(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub id: String,
    pub granted_per_day: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub pool: String,
    /// Including reasoning tokens. The liability of a request that names no
    /// cap of its own.
    pub max_output_tokens: u64,
    /// Only for a model whose encoding is known to cover all 256 byte values.
    /// GPT-5.6's is unpublished, so it stays false there.
    #[serde(default)]
    pub byte_level_encoding_known: bool,
}

/// OpenRouter's free allowance is a number of requests a day, not tokens, so
/// this section is denominated in requests throughout.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterSection {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "openrouter_base_url")]
    pub base_url: String,
    pub api_key_env: String,
    /// Only a management key can list BYOK endpoints. Without one, BYOK is
    /// recorded as unchecked rather than assumed absent.
    #[serde(default)]
    pub management_key_env: Option<String>,
    /// 50 a day on an account that has never bought credits, 1,000 after $10.
    #[serde(default = "fifty")]
    pub daily_request_limit: u64,
    /// OpenRouter answers successful requests with no rate-limit headers, so
    /// the short window is enforced here before sending, not after a 429.
    #[serde(default = "twenty")]
    pub max_requests_per_window: u32,
    #[serde(default = "sixty")]
    pub window_seconds: i64,
    #[serde(default = "nine_hundred_u64")]
    pub account_ttl_seconds: u64,
    /// Ordered. The first whose capabilities cover a request is used, so the
    /// catch-all router belongs last.
    #[serde(rename = "model", default)]
    pub models: Vec<OpenRouterModelSection>,
}

/// What a `:free` model was **measured** to accept. Not what it advertises:
/// the providers behind one model id differ, and `supported_parameters` does
/// not describe Codex's custom or namespace tool shapes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterModelSection {
    pub id: String,
    #[serde(default = "yes")]
    pub function_tools: bool,
    #[serde(default)]
    pub custom_tools: bool,
    #[serde(default)]
    pub namespace_tools: bool,
    #[serde(default)]
    pub structured_outputs: bool,
}

fn yes() -> bool {
    true
}
fn openrouter_base_url() -> String {
    "https://openrouter.ai/api/v1".to_string()
}
fn fifty() -> u64 {
    50
}
fn twenty() -> u32 {
    20
}
fn sixty() -> i64 {
    60
}

fn four() -> u64 {
    4
}
fn five() -> i64 {
    5
}
fn ten() -> u64 {
    10
}
fn one_twenty() -> u64 {
    120
}
fn three_hundred() -> i64 {
    300
}
fn nine_hundred() -> i64 {
    900
}
fn nine_hundred_u64() -> u64 {
    900
}

/// The request counter's key for the OpenRouter route. It is not a token
/// pool, and nothing denominated in tokens may use this name.
pub const OPENROUTER_POOL: &str = "openrouter:free";

/// One model's place in the inventory.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub pool_id: String,
    pub spec: ModelSpec,
}

/// The configuration, checked and turned into the shapes the runtime uses.
pub struct Resolved {
    pub bind: SocketAddr,
    pub upstream: UpstreamConfig,
    pub admin_key: String,
    pub usage_base_url: String,
    pub ledger: LedgerConfig,
    pub clock_policy: ClockPolicy,
    pub clock_refresh: Duration,
    pub data_sharing_ttl: i64,
    pub safety_budget_divisor: u64,
    /// Pool grants for a day, in the order the ledger is given them.
    pub grants: Vec<(String, u64)>,
    pub catalog: HashMap<String, CatalogEntry>,
    /// `None` when the route is absent or switched off.
    pub openrouter: Option<ResolvedOpenRouter>,
}

/// One `:free` model and what it was measured to accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRouterModel {
    pub id: String,
    pub function_tools: bool,
    pub custom_tools: bool,
    pub namespace_tools: bool,
    pub structured_outputs: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedOpenRouter {
    pub base_url: String,
    pub api_key: String,
    pub management_key: Option<String>,
    /// The request counter's key. Not a token pool; the two are different
    /// resources and never share a counter.
    pub pool_id: String,
    pub daily_request_limit: u64,
    pub window: RequestWindow,
    pub account_ttl: i64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub models: Vec<OpenRouterModel>,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Reads the credentials from the environment and checks every value that
    /// startup depends on.
    pub fn resolve(self) -> Result<Resolved, ConfigError> {
        let bind: SocketAddr = self
            .server
            .bind
            .parse()
            .map_err(|_| ConfigError::BadBindAddress(self.server.bind.clone()))?;
        if !bind.ip().is_loopback() {
            return Err(ConfigError::NotLoopback);
        }

        let api_key = credential(&self.upstream.api_key_env)?;
        let admin_key = credential(&self.upstream.admin_key_env)?;

        if self.pools.is_empty() || self.models.is_empty() {
            return Err(ConfigError::Empty);
        }
        let mut grants: Vec<(String, u64)> = Vec::with_capacity(self.pools.len());
        for pool in &self.pools {
            if grants.iter().any(|(id, _)| *id == pool.id) {
                return Err(ConfigError::DuplicatePool(pool.id.clone()));
            }
            // Token pools and the request counter must never share a name: one
            // is denominated in tokens and the other in requests.
            if pool.id == OPENROUTER_POOL {
                return Err(ConfigError::PoolNameReserved(pool.id.clone()));
            }
            grants.push((pool.id.clone(), pool.granted_per_day));
        }

        let mut catalog = HashMap::with_capacity(self.models.len());
        for model in &self.models {
            if model.max_output_tokens == 0 {
                return Err(ConfigError::ZeroOutput(model.id.clone()));
            }
            if !grants.iter().any(|(id, _)| *id == model.pool) {
                return Err(ConfigError::UnknownPool {
                    model: model.id.clone(),
                    pool: model.pool.clone(),
                });
            }
            let entry = CatalogEntry {
                pool_id: model.pool.clone(),
                spec: ModelSpec {
                    max_output_tokens: model.max_output_tokens,
                    byte_level_encoding_known: model.byte_level_encoding_known,
                },
            };
            if catalog.insert(model.id.clone(), entry).is_some() {
                return Err(ConfigError::DuplicateModel(model.id.clone()));
            }
        }

        let openrouter = match &self.openrouter {
            Some(section) if section.enabled => {
                if section.models.is_empty() {
                    return Err(ConfigError::NoOpenRouterModels);
                }
                if section.max_requests_per_window == 0 || section.window_seconds <= 0 {
                    return Err(ConfigError::EmptyOpenRouterWindow);
                }
                let api_key = credential(&section.api_key_env)?;
                let management_key = match &section.management_key_env {
                    Some(name) => Some(credential(name)?),
                    None => None,
                };
                Some(ResolvedOpenRouter {
                    base_url: section.base_url.clone(),
                    api_key,
                    management_key,
                    pool_id: OPENROUTER_POOL.to_string(),
                    daily_request_limit: section.daily_request_limit,
                    window: RequestWindow {
                        max_in_window: section.max_requests_per_window,
                        window_seconds: section.window_seconds,
                    },
                    account_ttl: i64::try_from(section.account_ttl_seconds).unwrap_or(i64::MAX),
                    connect_timeout: Duration::from_secs(self.upstream.connect_timeout_seconds),
                    read_timeout: Duration::from_secs(self.upstream.read_timeout_seconds),
                    models: section
                        .models
                        .iter()
                        .map(|model| OpenRouterModel {
                            id: model.id.clone(),
                            function_tools: model.function_tools,
                            custom_tools: model.custom_tools,
                            namespace_tools: model.namespace_tools,
                            structured_outputs: model.structured_outputs,
                        })
                        .collect(),
                })
            }
            _ => None,
        };

        let clock_policy = ClockPolicy::new(
            self.clock.reading_uncertainty_seconds,
            self.clock.open_delay_seconds,
            self.clock.max_reading_age_seconds,
        )?;

        // The Usage API is not under the version prefix the inference base URL
        // carries, but it shares its origin.
        let usage_base_url = self.upstream.base_url.trim_end_matches('/').to_string();

        Ok(Resolved {
            bind,
            upstream: UpstreamConfig {
                base_url: self.upstream.base_url.clone(),
                api_key,
                project: self.upstream.project.clone(),
                connect_timeout: Duration::from_secs(self.upstream.connect_timeout_seconds),
                read_timeout: Duration::from_secs(self.upstream.read_timeout_seconds),
            },
            admin_key,
            usage_base_url,
            ledger: LedgerConfig {
                ledger_path: self.ledger.path.clone(),
                external_hwm_path: self.ledger.external_record_path.clone(),
                lock_dir: self.ledger.lock_dir.clone(),
                organization: self.ledger.organization.clone(),
                pools: self.pools.iter().map(|pool| pool.id.clone()).collect(),
                allow_same_volume_external_record: self.ledger.allow_same_volume_external_record,
            },
            clock_policy,
            clock_refresh: Duration::from_secs(self.clock.refresh_seconds),
            data_sharing_ttl: i64::try_from(self.safety.data_sharing_ttl_seconds)
                .unwrap_or(i64::MAX),
            safety_budget_divisor: self.safety.liability_budget_divisor.max(1),
            grants,
            catalog,
            openrouter,
        })
    }
}

fn credential(variable: &str) -> Result<String, ConfigError> {
    match std::env::var(variable) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ConfigError::MissingCredential(variable.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[server]
bind = "127.0.0.1:8787"

[upstream]
base_url = "https://api.openai.com/v1"
api_key_env = "QM_TEST_KEY"
admin_key_env = "QM_TEST_ADMIN"

[ledger]
path = "state/ledger.db"
external_record_path = "state/external/hwm"
lock_dir = "state/locks"
organization = "org-test"
allow_same_volume_external_record = true

[[pool]]
id = "openai:small"
granted_per_day = 2500000

[[model]]
id = "gpt-5.6-terra"
pool = "openai:small"
max_output_tokens = 128000
"#;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("the sample parses")
    }

    /// `Resolved` holds credentials, so it deliberately has no `Debug`; this
    /// takes the error without asking for one.
    fn refuse(config: Config) -> ConfigError {
        match config.resolve() {
            Err(error) => error,
            Ok(_) => panic!("expected the configuration to be refused"),
        }
    }

    /// The environment is process-wide, so the tests that need credentials
    /// set them in one place.
    fn with_credentials<T>(body: impl FnOnce() -> T) -> T {
        // SAFETY: single-threaded use within this test module.
        unsafe {
            std::env::set_var("QM_TEST_KEY", "sk-test");
            std::env::set_var("QM_TEST_ADMIN", "sk-admin-test");
            std::env::set_var("QM_TEST_OR", "sk-or-test");
        }
        body()
    }

    #[test]
    fn the_sample_resolves() {
        let resolved = with_credentials(|| parse(SAMPLE).resolve().expect("resolves"));
        assert_eq!(resolved.bind.port(), 8787);
        assert_eq!(
            resolved.grants,
            vec![("openai:small".to_string(), 2_500_000)]
        );
        let entry = resolved
            .catalog
            .get("gpt-5.6-terra")
            .expect("in the catalog");
        assert_eq!(entry.pool_id, "openai:small");
        assert_eq!(entry.spec.max_output_tokens, 128_000);
        assert!(
            !entry.spec.byte_level_encoding_known,
            "an unpublished encoding is not assumed"
        );
        assert_eq!(resolved.ledger.pools, vec!["openai:small".to_string()]);
    }

    #[test]
    fn an_unknown_key_stops_startup() {
        let text = SAMPLE.replace("bind =", "bnid =");
        assert!(toml::from_str::<Config>(&text).is_err());
        let text = format!("{SAMPLE}\n[extra]\nwhat = 1\n");
        assert!(toml::from_str::<Config>(&text).is_err());
    }

    #[test]
    fn a_non_loopback_bind_is_refused() {
        let text = SAMPLE.replace("127.0.0.1:8787", "0.0.0.0:8787");
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::NotLoopback));
    }

    #[test]
    fn a_model_must_name_a_configured_pool() {
        let text = SAMPLE.replace("pool = \"openai:small\"", "pool = \"openai:large\"");
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::UnknownPool { .. }), "{error}");
    }

    #[test]
    fn a_missing_credential_stops_startup() {
        let text = SAMPLE.replace("QM_TEST_KEY", "QM_TEST_ABSENT");
        let error = refuse(parse(&text));
        assert!(
            matches!(&error, ConfigError::MissingCredential(name) if name == "QM_TEST_ABSENT"),
            "{error}"
        );
    }

    const OPENROUTER: &str = r#"
[openrouter]
api_key_env = "QM_TEST_OR"
daily_request_limit = 50
max_requests_per_window = 20
window_seconds = 60

[[openrouter.model]]
id = "nex-agi/nex-n2.5-pro:free"
namespace_tools = true

[[openrouter.model]]
id = "openrouter/free"
namespace_tools = true
"#;

    #[test]
    fn the_openrouter_route_resolves_with_its_measured_capabilities() {
        let resolved =
            with_credentials(|| parse(&format!("{SAMPLE}{OPENROUTER}")).resolve().unwrap());
        let route = resolved.openrouter.expect("the route is configured");
        assert_eq!(route.pool_id, OPENROUTER_POOL);
        assert_eq!(route.daily_request_limit, 50);
        assert_eq!(route.window.max_in_window, 20);
        assert_eq!(route.window.window_seconds, 60);
        assert_eq!(route.management_key, None, "BYOK is simply not checked");
        assert_eq!(route.models.len(), 2);
        assert_eq!(route.models[0].id, "nex-agi/nex-n2.5-pro:free");
        assert!(route.models[0].function_tools);
        assert!(
            !route.models[0].custom_tools,
            "custom tools default to unavailable until measured"
        );
        assert!(route.models[0].namespace_tools);
        assert_eq!(
            route.models[1].id, "openrouter/free",
            "the catch-all router comes last"
        );
    }

    #[test]
    fn an_absent_or_disabled_openrouter_route_resolves_to_none() {
        let resolved = with_credentials(|| parse(SAMPLE).resolve().unwrap());
        assert!(resolved.openrouter.is_none());

        let off = format!(
            "{SAMPLE}{}",
            OPENROUTER.replace("[openrouter]", "[openrouter]\nenabled = false")
        );
        let resolved = with_credentials(|| parse(&off).resolve().unwrap());
        assert!(resolved.openrouter.is_none());
    }

    #[test]
    fn an_enabled_route_with_no_models_is_refused() {
        let text = format!("{SAMPLE}\n[openrouter]\napi_key_env = \"QM_TEST_OR\"\n");
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::NoOpenRouterModels), "{error}");
    }

    #[test]
    fn a_token_pool_may_not_take_the_request_counters_name() {
        let text = SAMPLE.replace("id = \"openai:small\"", "id = \"openrouter:free\"");
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::PoolNameReserved(_)), "{error}");
    }

    #[test]
    fn duplicates_are_refused() {
        let text = format!("{SAMPLE}\n[[pool]]\nid = \"openai:small\"\ngranted_per_day = 1\n");
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::DuplicatePool(_)), "{error}");

        let text = format!(
            "{SAMPLE}\n[[model]]\nid = \"gpt-5.6-terra\"\npool = \"openai:small\"\nmax_output_tokens = 1\n"
        );
        let error = with_credentials(|| refuse(parse(&text)));
        assert!(matches!(error, ConfigError::DuplicateModel(_)), "{error}");
    }
}
