//! Table-driven OpenAI-compatible provider.
//!
//! All providers that speak the OpenAI Chat Completions protocol share a single
//! implementation ([`OpenAiCompatProvider`]).  The only per-provider knowledge
//! lives in a static [`OpenAiCompatDef`] record — adding a new vendor requires
//! **one entry in `COMPAT_DEFS`** and nothing else.
//!
//! # Provider table
//!
//! Current entries (in priority / preference order):
//!
//! | name        | vendor                  | API key env var       |
//! |-------------|-------------------------|-----------------------|
//! | deepseek    | DeepSeek                | DEEPSEEK_API_KEY      |
//! | groq        | Groq                    | GROQ_API_KEY          |
//! | openrouter  | OpenRouter (gateway)    | OPENROUTER_API_KEY    |
//! | gemini      | Google Gemini           | GEMINI_API_KEY        |
//! | mistral     | Mistral AI              | MISTRAL_API_KEY       |
//! | xai         | xAI (Grok)              | XAI_API_KEY           |
//! | qwen        | Alibaba DashScope/Qwen  | DASHSCOPE_API_KEY     |
//! | glm         | Zhipu AI / Z.AI (GLM)  | ZAI_API_KEY           |
//! | moonshot    | Moonshot AI (Kimi)      | MOONSHOT_API_KEY      |
//! | minimax     | MiniMax                 | MINIMAX_API_KEY       |

use adaclaw_core::provider::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use secrecy::{ExposeSecret, Secret};
use serde_json::Value;

use crate::error::ProviderError;
use crate::registry::ProviderSpec;

// ── Per-provider static metadata ────────────────────────────────────────────

/// Static definition for a single OpenAI-compatible provider.
///
/// All fields are `'static` so the table can live as a `const`/`static` slice.
pub struct OpenAiCompatDef {
    /// Config / registry name (e.g. `"deepseek"`).
    pub name: &'static str,
    /// Model-name aliases that auto-select this provider.
    pub aliases: &'static [&'static str],
    /// Default base URL (overridable at runtime).
    pub default_base_url: &'static str,
    pub capabilities: ProviderCapabilities,
    /// If `Some(t)`, the `temperature` parameter is clamped to at least `t`.
    /// Kimi K2.5 requires `temperature >= 1.0`.
    pub min_temperature: Option<f64>,
}

// ── Provider table ───────────────────────────────────────────────────────────

/// All OpenAI-compatible providers.  Order controls alias-match priority.
pub static COMPAT_DEFS: &[OpenAiCompatDef] = &[
    // ── International ───────────────────────────────────────────────────────
    OpenAiCompatDef {
        name: "deepseek",
        aliases: &["deepseek-chat", "deepseek-reasoner", "ds"],
        default_base_url: "https://api.deepseek.com/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "groq",
        aliases: &["groq"],
        default_base_url: "https://api.groq.com/openai/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "openrouter",
        aliases: &["openrouter"],
        default_base_url: "https://openrouter.ai/api/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "gemini",
        // Google exposes Gemini through an OpenAI-compatible endpoint.
        aliases: &[
            "gemini",
            "gemini-2.0-flash",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
        ],
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "mistral",
        aliases: &["mistral", "mistral-large", "codestral"],
        default_base_url: "https://api.mistral.ai/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "xai",
        // xAI API is fully OpenAI-compatible.
        aliases: &[
            "xai",
            "grok",
            "grok-2",
            "grok-2-vision",
            "grok-beta",
            "grok-3",
        ],
        default_base_url: "https://api.x.ai/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true, // grok-2-vision supports image input
            streaming: true,
        },
        min_temperature: None,
    },
    // ── Chinese providers ────────────────────────────────────────────────────
    OpenAiCompatDef {
        name: "qwen",
        // Alibaba DashScope — Qwen models.
        // Also reachable via the alias "dashscope".
        aliases: &[
            "qwen",
            "dashscope",
            "qwen-max",
            "qwen-plus",
            "qwen-turbo",
            "qwen-long",
        ],
        default_base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "glm",
        // Zhipu AI / Z.AI — GLM models.
        // Also reachable via the aliases "zhipu" and "zai".
        aliases: &[
            "glm", "zhipu", "zai", "glm-4", "glm-4.5", "glm-4.6", "glm-4.7", "glm-5",
        ],
        default_base_url: "https://api.z.ai/api/paas/v4",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: true, // GLM-4V variants support vision
            streaming: true,
        },
        min_temperature: None,
    },
    OpenAiCompatDef {
        name: "moonshot",
        // Moonshot AI — Kimi models.
        // Also reachable via the alias "kimi".
        aliases: &["moonshot", "kimi", "kimi-k2.5", "kimi-latest"],
        default_base_url: "https://api.moonshot.cn/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        // Kimi K2.5 API enforces temperature >= 1.0.
        min_temperature: Some(1.0),
    },
    OpenAiCompatDef {
        name: "minimax",
        aliases: &["minimax", "MiniMax-M2", "MiniMax-M2.1", "MiniMax-M2.5"],
        default_base_url: "https://api.minimax.io/v1",
        capabilities: ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
            streaming: true,
        },
        min_temperature: None,
    },
];

// ── Runtime provider instance ────────────────────────────────────────────────

pub struct OpenAiCompatProvider {
    def: &'static OpenAiCompatDef,
    key: Option<Secret<String>>,
    base_url: String,
    client: Client,
}

impl OpenAiCompatProvider {
    pub fn new(def: &'static OpenAiCompatDef, key: Option<&str>, url: Option<&str>) -> Self {
        Self {
            def,
            key: key.map(|s| Secret::new(s.to_string())),
            base_url: url
                .unwrap_or(def.default_base_url)
                .trim_end_matches('/')
                .to_string(),
            client: Client::new(),
        }
    }

    fn build_messages(req: &ChatRequest<'_>) -> Vec<Value> {
        let mut msgs = Vec::new();
        if let Some(sys) = req.system {
            msgs.push(serde_json::json!({"role": "system", "content": sys}));
        }
        for m in req.messages {
            msgs.push(serde_json::json!({"role": m.role, "content": m.content}));
        }
        msgs
    }

    /// Strip `<think>…</think>` tags from `content`.
    ///
    /// Some models (DeepSeek-R1, QwQ, MiniMax) embed chain-of-thought
    /// reasoning inside `<think>` tags in the `content` field rather than
    /// using a dedicated `reasoning_content` field.
    ///
    /// Returns `(visible_content, thinking_content)`.
    fn strip_think_tags(content: &str) -> (String, Option<String>) {
        let mut visible = String::new();
        let mut thinking = String::new();
        let mut remaining = content;

        loop {
            match remaining.find("<think>") {
                Some(start) => {
                    visible.push_str(&remaining[..start]);
                    let after_open = &remaining[start + "<think>".len()..];
                    match after_open.find("</think>") {
                        Some(end) => {
                            thinking.push_str(&after_open[..end]);
                            remaining = &after_open[end + "</think>".len()..];
                        }
                        None => {
                            // Unclosed <think> — treat remainder as reasoning
                            thinking.push_str(after_open);
                            break;
                        }
                    }
                }
                None => {
                    visible.push_str(remaining);
                    break;
                }
            }
        }

        let thinking_opt = if thinking.is_empty() {
            None
        } else {
            Some(thinking.trim_start().to_string())
        };
        (visible.trim_start().to_string(), thinking_opt)
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        self.def.name
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.def.capabilities.clone()
    }

    async fn chat(&self, req: ChatRequest<'_>, model: &str, temp: f64) -> Result<ChatResponse> {
        let messages = Self::build_messages(&req);

        // Respect per-provider minimum temperature constraint.
        let effective_temp = self
            .def
            .min_temperature
            .map(|min| temp.max(min))
            .unwrap_or(temp);

        let body = serde_json::json!({
            "model": model,
            "messages": messages,
            "temperature": effective_temp,
        });

        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);

        if let Some(key) = &self.key {
            builder = builder.header("Authorization", format!("Bearer {}", key.expose_secret()));
        }

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::Error::new(ProviderError::from_status(
                status,
                &text,
                retry_after,
            )));
        }

        let data: Value = resp.json().await?;
        let message = &data["choices"][0]["message"];

        let raw_content = message["content"].as_str().unwrap_or("").to_string();

        // 1. Prefer explicit `reasoning_content` field (DeepSeek-R1 etc.)
        let explicit_reasoning = message["reasoning_content"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        // 2. Fall back to stripping <think> tags from content.
        let (content, reasoning_content) = if explicit_reasoning.is_some() {
            (raw_content, explicit_reasoning)
        } else {
            let (vis, think) = Self::strip_think_tags(&raw_content);
            if think.is_some() {
                (vis, think)
            } else {
                (raw_content, None)
            }
        };

        Ok(ChatResponse {
            content,
            reasoning_content,
        })
    }

    async fn chat_with_system(
        &self,
        system: Option<&str>,
        msg: &str,
        model: &str,
        temp: f64,
    ) -> Result<String> {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: msg.to_string(),
        }];
        let req = ChatRequest {
            messages: &messages,
            system,
        };
        Ok(self.chat(req, model, temp).await?.content)
    }

    /// Discover available models via `GET {base_url}/models`.
    ///
    /// Most OpenAI-compatible providers expose this endpoint; those that don't
    /// will return a non-2xx response which is silently converted to `Ok(None)`.
    async fn list_models(&self) -> Result<Option<Vec<String>>> {
        let mut builder = self.client.get(format!("{}/models", self.base_url));

        if let Some(key) = &self.key {
            builder = builder.header("Authorization", format!("Bearer {}", key.expose_secret()));
        }

        let resp = match builder.send().await {
            Ok(r) => r,
            Err(_) => return Ok(None), // unreachable host — not an error for callers
        };

        if !resp.status().is_success() {
            return Ok(None); // endpoint not supported by this provider
        }

        let data: Value = resp.json().await?;
        let models: Vec<String> = data["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if models.is_empty() {
            Ok(None)
        } else {
            Ok(Some(models))
        }
    }
}

// ── ProviderSpec constants for the registry ──────────────────────────────────

/// Generate a `ProviderSpec` from a `COMPAT_DEFS` entry by name.
///
/// Called once during static initialization; panics if `name` is not found
/// (programming error, not a runtime condition).
pub fn spec_for(name: &'static str) -> ProviderSpec {
    let def = COMPAT_DEFS
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("openai_compat: no def for '{name}'"));

    ProviderSpec {
        name: def.name,
        aliases: def.aliases,
        local: false,
        capabilities: def.capabilities.clone(),
        // `def` is `&'static`, so the closure is `'static` too.
        factory: Box::new(move |key, url| {
            Box::new(OpenAiCompatProvider::new(def, key, url))
                as Box<dyn adaclaw_core::provider::Provider>
        }),
    }
}
