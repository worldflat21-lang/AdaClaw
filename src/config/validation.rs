//! Field-level semantic validation for [`Config`].
//!
//! Validation is intentionally separate from deserialization so that TOML
//! parse errors ("expected string at line 12") remain distinct from semantic
//! errors ("autonomy_level must be 'readonly', 'supervised', or 'full'").
//!
//! All errors are collected and returned at once rather than stopping at the
//! first problem — this lets users fix everything in a single edit cycle.
//!
//! # Usage
//!
//! ```ignore
//! let cfg = Config::load_from_file("config.toml")?;
//! let errors = config::validation::validate(&cfg);
//! if !errors.is_empty() {
//!     for e in &errors { eprintln!("  • {e}"); }
//!     anyhow::bail!("Config has {} error(s) — see above.", errors.len());
//! }
//! ```

use std::collections::HashSet;

use super::schema::Config;

// ── Public types ───────────────────────────────────────────────────────────────

/// A single semantic validation error, pointing to the field and describing
/// the problem in plain language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// TOML-style dotted path to the offending field
    /// (e.g. `"agents.assistant.model"` or `"routing[2].agent"`).
    pub field: String,
    /// Human-readable explanation of what is wrong and how to fix it.
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "config.{}: {}", self.field, self.message)
    }
}

// ── Entry point ────────────────────────────────────────────────────────────────

/// Validate `cfg` and return every semantic error found.
///
/// An empty `Vec` means the config is valid.
pub fn validate(cfg: &Config) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    validate_agents(cfg, &mut errors);
    validate_routing(cfg, &mut errors);
    validate_security(cfg, &mut errors);
    validate_channels(cfg, &mut errors);
    validate_memory(cfg, &mut errors);
    validate_observability(cfg, &mut errors);

    errors
}

// ── Per-section validators ─────────────────────────────────────────────────────

fn validate_agents(cfg: &Config, errors: &mut Vec<ValidationError>) {
    if cfg.agents.is_empty() {
        errors.push(ValidationError {
            field: "agents".to_string(),
            message: "at least one agent must be defined \
                      (e.g. add an [agents.assistant] section)"
                .to_string(),
        });
        return; // nothing more to check
    }

    let agent_names: Vec<&str> = cfg.agents.keys().map(String::as_str).collect();

    for (id, agent) in &cfg.agents {
        let prefix = format!("agents.{id}");

        // model is the only truly required field (provider + temperature have defaults)
        if agent.model.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("{prefix}.model"),
                message: format!(
                    "model is required (e.g. model = \"gpt-4o\"). \
                     Available providers: {}",
                    cfg.providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                        .as_str()
                        .to_string()
                        .or_default_str("none configured — add a [providers.<name>] section")
                ),
            });
        }

        // temperature must be in [0.0, 2.0] — range accepted by all major providers
        if agent.temperature < 0.0 || agent.temperature > 2.0 {
            errors.push(ValidationError {
                field: format!("{prefix}.temperature"),
                message: format!(
                    "must be between 0.0 and 2.0, got {}",
                    agent.temperature
                ),
            });
        }

        // max_iterations must be ≥ 1; 0 means the agent can never call any tool
        if agent.max_iterations == 0 {
            errors.push(ValidationError {
                field: format!("{prefix}.max_iterations"),
                message: "must be at least 1 (0 means no tool calls are ever made)".to_string(),
            });
        }

        // Every target in subagents.allow must point to a known agent
        for target in &agent.subagents.allow {
            if !cfg.agents.contains_key(target.as_str()) {
                errors.push(ValidationError {
                    field: format!("{prefix}.subagents.allow"),
                    message: format!(
                        "references unknown agent \"{target}\" \
                         (known agents: {known})",
                        known = agent_names.join(", ")
                    ),
                });
            }
        }

        // provider cross-check: if providers map is non-empty, warn about unknown provider
        if !cfg.providers.is_empty()
            && !cfg.providers.contains_key(&agent.provider)
        {
            errors.push(ValidationError {
                field: format!("{prefix}.provider"),
                message: format!(
                    "references provider \"{prov}\" which is not configured \
                     (configured providers: {known}). \
                     Add a [providers.{prov}] section or set \
                     ADACLAW_{upper}_API_KEY.",
                    prov = agent.provider,
                    known = cfg
                        .providers
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    upper = agent.provider.to_uppercase(),
                ),
            });
        }
    }
}

fn validate_routing(cfg: &Config, errors: &mut Vec<ValidationError>) {
    let agent_ids: HashSet<&str> = cfg.agents.keys().map(String::as_str).collect();
    let mut has_default = false;

    for (i, rule) in cfg.routing.iter().enumerate() {
        let prefix = format!("routing[{i}]");

        if rule.agent.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("{prefix}.agent"),
                message: "routing rule must specify an agent name".to_string(),
            });
        } else if !agent_ids.is_empty() && !agent_ids.contains(rule.agent.as_str()) {
            errors.push(ValidationError {
                field: format!("{prefix}.agent"),
                message: format!(
                    "references unknown agent \"{}\" (known agents: {})",
                    rule.agent,
                    agent_ids
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }

        // A rule is "default" if default=true AND no other matcher is specified
        if rule.default
            && rule.channel_pattern.is_none()
            && rule.sender_id.is_none()
            && rule.sender_name.is_none()
        {
            has_default = true;
        }
    }

    // Soft warning: routing rules exist but nothing catches unmatched messages
    if !has_default && !cfg.routing.is_empty() && cfg.agents.len() > 1 {
        errors.push(ValidationError {
            field: "routing".to_string(),
            message: "no default routing rule — add a catch-all:\n  \
                      [[routing]]\n  \
                      default = true\n  \
                      agent = \"assistant\""
                .to_string(),
        });
    }
}

fn validate_security(cfg: &Config, errors: &mut Vec<ValidationError>) {
    const VALID_LEVELS: &[&str] = &["readonly", "supervised", "full"];

    if !VALID_LEVELS.contains(&cfg.security.autonomy_level.as_str()) {
        errors.push(ValidationError {
            field: "security.autonomy_level".to_string(),
            message: format!(
                "must be one of {:?}, got \"{}\"",
                VALID_LEVELS, cfg.security.autonomy_level
            ),
        });
    }
}

fn validate_channels(cfg: &Config, errors: &mut Vec<ValidationError>) {
    const VALID_KINDS: &[&str] = &[
        "telegram",
        "cli",
        "discord",
        "slack",
        "dingtalk",
        "feishu",
        "wechat_work",
        "webhook",
        "whatsapp",
        "email",
        "matrix",
    ];

    for (name, ch) in &cfg.channels {
        let prefix = format!("channels.{name}");

        // ── kind ──────────────────────────────────────────────────────────────
        if ch.kind.trim().is_empty() {
            errors.push(ValidationError {
                field: format!("{prefix}.kind"),
                message: format!(
                    "channel kind is required — must be one of: {}",
                    VALID_KINDS.join(", ")
                ),
            });
            continue; // can't do channel-specific checks without knowing kind
        }

        if !VALID_KINDS.contains(&ch.kind.as_str()) {
            errors.push(ValidationError {
                field: format!("{prefix}.kind"),
                message: format!(
                    "unknown channel kind \"{}\". Valid values: {}",
                    ch.kind,
                    VALID_KINDS.join(", ")
                ),
            });
            continue;
        }

        // ── Per-kind required fields ──────────────────────────────────────────
        match ch.kind.as_str() {
            "telegram" => {
                // Token can also come from env var
                let env_has_token = std::env::var("ADACLAW_TELEGRAM_TOKEN").is_ok();
                if ch.token.is_none() && !env_has_token {
                    errors.push(ValidationError {
                        field: format!("{prefix}.token"),
                        message: "Telegram channel requires a bot token. \
                                  Set `token = \"...\"` or the \
                                  ADACLAW_TELEGRAM_TOKEN environment variable."
                            .to_string(),
                    });
                }
            }

            "discord" => {
                if ch.token.is_none() {
                    errors.push(ValidationError {
                        field: format!("{prefix}.token"),
                        message: "Discord channel requires a bot token \
                                  (starts with \"Bot \")."
                            .to_string(),
                    });
                }
            }

            "slack" => {
                if ch.token.is_none() {
                    errors.push(ValidationError {
                        field: format!("{prefix}.token"),
                        message: "Slack channel requires a bot token (xoxb-...)."
                            .to_string(),
                    });
                }
            }

            "feishu" => {
                let missing_app_id = ch
                    .extra
                    .get("app_id")
                    .is_none_or(|s| s.trim().is_empty());
                let missing_app_secret = ch
                    .extra
                    .get("app_secret")
                    .is_none_or(|s| s.trim().is_empty());

                if missing_app_id {
                    errors.push(ValidationError {
                        field: format!("{prefix}.extra.app_id"),
                        message: "Feishu channel requires extra.app_id \
                                  (found in the Feishu Open Platform → App Credentials)."
                            .to_string(),
                    });
                }
                if missing_app_secret {
                    errors.push(ValidationError {
                        field: format!("{prefix}.extra.app_secret"),
                        message: "Feishu channel requires extra.app_secret."
                            .to_string(),
                    });
                }
            }

            "wechat_work" => {
                if ch.token.is_none() {
                    errors.push(ValidationError {
                        field: format!("{prefix}.token"),
                        message: "WeCom channel requires `token` (企业微信-接收消息-Token)."
                            .to_string(),
                    });
                }
                let missing_aes = ch
                    .extra
                    .get("encoding_aes_key")
                    .is_none_or(|s| s.trim().is_empty());
                if missing_aes {
                    errors.push(ValidationError {
                        field: format!("{prefix}.extra.encoding_aes_key"),
                        message: "WeCom channel requires extra.encoding_aes_key \
                                  (43-character Base64 key from WeCom dashboard)."
                            .to_string(),
                    });
                }
            }

            "whatsapp" => {
                // token field holds the Graph API access token
                if ch.token.is_none() {
                    errors.push(ValidationError {
                        field: format!("{prefix}.token"),
                        message: "WhatsApp channel requires `token` \
                                  (Meta Graph API access token, starts with \"EAA...\")."
                            .to_string(),
                    });
                }
            }

            // "cli", "dingtalk", "webhook", "email", "matrix" — no mandatory top-level fields
            _ => {}
        }
    }
}

fn validate_memory(cfg: &Config, errors: &mut Vec<ValidationError>) {
    const VALID_BACKENDS: &[&str] = &["sqlite", "markdown", "none"];
    const VALID_EMBED: &[&str] = &["none", "fastembed", "openai"];

    if !VALID_BACKENDS.contains(&cfg.memory.backend.as_str()) {
        errors.push(ValidationError {
            field: "memory.backend".to_string(),
            message: format!(
                "must be one of {:?}, got \"{}\"",
                VALID_BACKENDS, cfg.memory.backend
            ),
        });
    }

    if !VALID_EMBED.contains(&cfg.memory.embedding_provider.as_str()) {
        errors.push(ValidationError {
            field: "memory.embedding_provider".to_string(),
            message: format!(
                "must be one of {:?}, got \"{}\"",
                VALID_EMBED, cfg.memory.embedding_provider
            ),
        });
    }

    // OpenAI embedding needs an API key
    if cfg.memory.embedding_provider == "openai"
        && cfg.memory.embed_api_key.is_none()
        && std::env::var("OPENAI_API_KEY").is_err()
        && std::env::var("ADACLAW_OPENAI_API_KEY").is_err()
    {
        errors.push(ValidationError {
            field: "memory.embed_api_key".to_string(),
            message: "embedding_provider = \"openai\" requires `embed_api_key` \
                      (or set the OPENAI_API_KEY environment variable)."
                .to_string(),
        });
    }

    if !(0.0..=1.0).contains(&cfg.memory.vector_weight) {
        errors.push(ValidationError {
            field: "memory.vector_weight".to_string(),
            message: format!(
                "must be between 0.0 and 1.0, got {}",
                cfg.memory.vector_weight
            ),
        });
    }

    if !(0.0..=1.0).contains(&cfg.memory.keyword_weight) {
        errors.push(ValidationError {
            field: "memory.keyword_weight".to_string(),
            message: format!(
                "must be between 0.0 and 1.0, got {}",
                cfg.memory.keyword_weight
            ),
        });
    }
}

fn validate_observability(cfg: &Config, errors: &mut Vec<ValidationError>) {
    const VALID_BACKENDS: &[&str] = &["noop", "log", "prometheus"];

    if !VALID_BACKENDS.contains(&cfg.observability.backend.as_str()) {
        errors.push(ValidationError {
            field: "observability.backend".to_string(),
            message: format!(
                "must be one of {:?}, got \"{}\"",
                VALID_BACKENDS, cfg.observability.backend
            ),
        });
    }
}

// ── Small helper trait ─────────────────────────────────────────────────────────

/// Extension trait to give a default string when the receiver is empty.
trait OrDefaultStr {
    fn or_default_str(self, default: &str) -> String;
}

impl OrDefaultStr for String {
    fn or_default_str(self, default: &str) -> String {
        if self.is_empty() { default.to_string() } else { self }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{AgentConfig, ChannelConfig, RoutingRule};
    use std::collections::HashMap;

    /// Minimal config that passes all validation checks.
    fn valid_config() -> Config {
        let mut cfg = Config::default();

        cfg.providers.insert(
            "openai".to_string(),
            crate::config::schema::ProviderConfig {
                api_key: Some("sk-test".to_string()),
                ..Default::default()
            },
        );

        cfg.agents.insert(
            "assistant".to_string(),
            AgentConfig {
                model: "gpt-4o".to_string(),
                provider: "openai".to_string(),
                ..Default::default()
            },
        );

        cfg.routing = vec![RoutingRule {
            default: true,
            agent: "assistant".to_string(),
            channel_pattern: None,
            sender_id: None,
            sender_name: None,
        }];

        cfg
    }

    // ── Agents ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_valid_config_no_errors() {
        let cfg = valid_config();
        let errors = validate(&cfg);
        assert!(errors.is_empty(), "unexpected errors: {:#?}", errors);
    }

    #[test]
    fn test_empty_agents_map_is_an_error() {
        let mut cfg = valid_config();
        cfg.agents.clear();
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "agents"),
            "expected 'agents' error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_missing_model_is_an_error() {
        let mut cfg = valid_config();
        cfg.agents.insert(
            "no_model".to_string(),
            AgentConfig {
                model: "".to_string(),
                provider: "openai".to_string(),
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors
                .iter()
                .any(|e| e.field.contains("no_model") && e.field.ends_with(".model")),
            "expected model error for 'no_model', got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_temperature_below_zero_is_an_error() {
        let mut cfg = valid_config();
        cfg.agents.get_mut("assistant").unwrap().temperature = -0.1;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field.contains("temperature")),
            "expected temperature error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_temperature_above_two_is_an_error() {
        let mut cfg = valid_config();
        cfg.agents.get_mut("assistant").unwrap().temperature = 2.1;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field.contains("temperature")),
            "expected temperature error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_temperature_at_boundaries_is_ok() {
        let mut cfg = valid_config();
        cfg.agents.get_mut("assistant").unwrap().temperature = 0.0;
        assert!(validate(&cfg).is_empty());
        cfg.agents.get_mut("assistant").unwrap().temperature = 2.0;
        assert!(validate(&cfg).is_empty());
    }

    #[test]
    fn test_max_iterations_zero_is_an_error() {
        let mut cfg = valid_config();
        cfg.agents.get_mut("assistant").unwrap().max_iterations = 0;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field.contains("max_iterations")),
            "expected max_iterations error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_subagent_references_unknown_agent() {
        let mut cfg = valid_config();
        cfg.agents
            .get_mut("assistant")
            .unwrap()
            .subagents
            .allow
            .push("ghost".to_string());
        let errors = validate(&cfg);
        assert!(
            errors
                .iter()
                .any(|e| e.field.contains("subagents") && e.message.contains("ghost")),
            "expected subagents/ghost error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_subagent_referencing_self_is_allowed_structurally() {
        // The no-recursion rule is enforced at runtime (DelegateTool), not at config-load
        let mut cfg = valid_config();
        cfg.agents
            .get_mut("assistant")
            .unwrap()
            .subagents
            .allow
            .push("assistant".to_string());
        // Should not produce a validation error (self-delegation is blocked at runtime)
        let errors = validate(&cfg);
        assert!(
            !errors.iter().any(|e| e.field.contains("subagents")),
            "self-reference should not be a config-time error, got: {:#?}",
            errors
        );
    }

    // ── Routing ────────────────────────────────────────────────────────────────

    #[test]
    fn test_routing_references_unknown_agent() {
        let mut cfg = valid_config();
        cfg.routing.push(RoutingRule {
            default: false,
            agent: "nobody".to_string(),
            channel_pattern: Some("telegram:*".to_string()),
            sender_id: None,
            sender_name: None,
        });
        let errors = validate(&cfg);
        assert!(
            errors
                .iter()
                .any(|e| e.field.contains("routing") && e.message.contains("nobody")),
            "expected routing/nobody error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_routing_with_empty_agent_is_an_error() {
        let mut cfg = valid_config();
        cfg.routing.push(RoutingRule {
            default: false,
            agent: "".to_string(),
            channel_pattern: None,
            sender_id: None,
            sender_name: None,
        });
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field.contains("routing") && e.field.contains("agent")),
            "expected routing agent error, got: {:#?}",
            errors
        );
    }

    // ── Security ───────────────────────────────────────────────────────────────

    #[test]
    fn test_invalid_autonomy_level() {
        let mut cfg = valid_config();
        cfg.security.autonomy_level = "turbo".to_string();
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "security.autonomy_level"),
            "expected autonomy_level error, got: {:#?}",
            errors
        );
        // Error message should list valid values
        let msg = &errors
            .iter()
            .find(|e| e.field == "security.autonomy_level")
            .unwrap()
            .message;
        assert!(
            msg.contains("readonly") && msg.contains("supervised") && msg.contains("full"),
            "message should list valid values, got: {msg}"
        );
    }

    #[test]
    fn test_valid_autonomy_levels() {
        for level in &["readonly", "supervised", "full"] {
            let mut cfg = valid_config();
            cfg.security.autonomy_level = level.to_string();
            let errors = validate(&cfg);
            assert!(
                !errors.iter().any(|e| e.field == "security.autonomy_level"),
                "level '{}' should be valid, got: {:#?}",
                level,
                errors
            );
        }
    }

    // ── Channels ───────────────────────────────────────────────────────────────

    #[test]
    fn test_channel_missing_kind_is_an_error() {
        let mut cfg = valid_config();
        cfg.channels.insert(
            "mystery".to_string(),
            ChannelConfig {
                kind: "".to_string(),
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "channels.mystery.kind"),
            "expected channel kind error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_channel_unknown_kind_is_an_error() {
        let mut cfg = valid_config();
        cfg.channels.insert(
            "bad_ch".to_string(),
            ChannelConfig {
                kind: "carrier_pigeon".to_string(),
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "channels.bad_ch.kind"),
            "expected channel kind error for 'carrier_pigeon', got: {:#?}",
            errors
        );
        let msg = &errors
            .iter()
            .find(|e| e.field == "channels.bad_ch.kind")
            .unwrap()
            .message;
        assert!(
            msg.contains("telegram") || msg.contains("discord"),
            "error message should list valid kinds, got: {msg}"
        );
    }

    #[test]
    fn test_telegram_channel_missing_token() {
        // Remove any env override to make the test deterministic
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("ADACLAW_TELEGRAM_TOKEN") };

        let mut cfg = valid_config();
        cfg.channels.insert(
            "tg".to_string(),
            ChannelConfig {
                kind: "telegram".to_string(),
                token: None,
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "channels.tg.token"),
            "expected token error for telegram, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_discord_channel_missing_token() {
        let mut cfg = valid_config();
        cfg.channels.insert(
            "dc".to_string(),
            ChannelConfig {
                kind: "discord".to_string(),
                token: None,
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "channels.dc.token"),
            "expected token error for discord, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_feishu_channel_missing_app_id_and_secret() {
        let mut cfg = valid_config();
        cfg.channels.insert(
            "fs".to_string(),
            ChannelConfig {
                kind: "feishu".to_string(),
                extra: HashMap::new(), // no app_id / app_secret
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        let has_app_id_err = errors.iter().any(|e| e.field.contains("app_id"));
        let has_secret_err = errors.iter().any(|e| e.field.contains("app_secret"));
        assert!(has_app_id_err, "expected app_id error, got: {:#?}", errors);
        assert!(has_secret_err, "expected app_secret error, got: {:#?}", errors);
    }

    #[test]
    fn test_wecom_channel_missing_token_and_aes_key() {
        let mut cfg = valid_config();
        cfg.channels.insert(
            "wc".to_string(),
            ChannelConfig {
                kind: "wechat_work".to_string(),
                token: None,
                extra: HashMap::new(),
                ..Default::default()
            },
        );
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "channels.wc.token"),
            "expected WeCom token error, got: {:#?}",
            errors
        );
        assert!(
            errors.iter().any(|e| e.field.contains("encoding_aes_key")),
            "expected WeCom aes_key error, got: {:#?}",
            errors
        );
    }

    // ── Memory ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_invalid_memory_backend() {
        let mut cfg = valid_config();
        cfg.memory.backend = "postgres".to_string();
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "memory.backend"),
            "expected memory.backend error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_invalid_embedding_provider() {
        let mut cfg = valid_config();
        cfg.memory.embedding_provider = "cohere".to_string();
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "memory.embedding_provider"),
            "expected embedding_provider error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_openai_embed_without_key_is_an_error() {
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("ADACLAW_OPENAI_API_KEY") };

        let mut cfg = valid_config();
        cfg.memory.embedding_provider = "openai".to_string();
        cfg.memory.embed_api_key = None;

        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "memory.embed_api_key"),
            "expected embed_api_key error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_vector_weight_out_of_range() {
        let mut cfg = valid_config();
        cfg.memory.vector_weight = 1.1;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "memory.vector_weight"),
            "expected vector_weight error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_keyword_weight_negative() {
        let mut cfg = valid_config();
        cfg.memory.keyword_weight = -0.1;
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "memory.keyword_weight"),
            "expected keyword_weight error, got: {:#?}",
            errors
        );
    }

    // ── Observability ──────────────────────────────────────────────────────────

    #[test]
    fn test_invalid_observability_backend() {
        let mut cfg = valid_config();
        cfg.observability.backend = "grafana".to_string();
        let errors = validate(&cfg);
        assert!(
            errors.iter().any(|e| e.field == "observability.backend"),
            "expected observability.backend error, got: {:#?}",
            errors
        );
    }

    #[test]
    fn test_valid_observability_backends() {
        for backend in &["noop", "log", "prometheus"] {
            let mut cfg = valid_config();
            cfg.observability.backend = backend.to_string();
            let errors = validate(&cfg);
            assert!(
                !errors.iter().any(|e| e.field == "observability.backend"),
                "backend '{}' should be valid, got: {:#?}",
                backend,
                errors
            );
        }
    }

    // ── Multiple errors at once ────────────────────────────────────────────────

    #[test]
    fn test_multiple_errors_collected_at_once() {
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("ADACLAW_TELEGRAM_TOKEN") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::remove_var("ADACLAW_OPENAI_API_KEY") };

        let mut cfg = valid_config();
        // Introduce three distinct errors simultaneously
        cfg.security.autonomy_level = "turbo".to_string();
        cfg.memory.backend = "redis".to_string();
        cfg.channels.insert(
            "tg2".to_string(),
            ChannelConfig {
                kind: "telegram".to_string(),
                token: None,
                ..Default::default()
            },
        );

        let errors = validate(&cfg);
        assert!(
            errors.len() >= 3,
            "expected at least 3 errors, got {}: {:#?}",
            errors.len(),
            errors
        );
    }
}
