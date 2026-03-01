use adaclaw_core::provider::{Provider, ProviderCapabilities};

/// Boxed factory function that creates a provider instance given an optional
/// API key and an optional base URL override.
pub type ProviderFactory =
    Box<dyn Fn(Option<&str>, Option<&str>) -> Box<dyn Provider> + Send + Sync>;

pub struct ProviderSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub local: bool,
    pub capabilities: ProviderCapabilities,
    pub factory: ProviderFactory,
}

/// Build the full provider registry.
///
/// Order matters for alias matching: the first spec whose `name` or `aliases`
/// matches the requested identifier wins.
///
/// Providers are grouped as follows:
///   1. Standalone implementations (OpenAI, Anthropic, Ollama) — require
///      custom HTTP behaviour beyond the vanilla OpenAI-compat protocol.
///   2. OpenAI-compatible table entries via `openai_compat::spec_for` —
///      adding a new vendor only requires a row in `COMPAT_DEFS`.
pub fn build_registry() -> Vec<ProviderSpec> {
    vec![
        // ── Standalone providers ─────────────────────────────────────────────
        crate::openai::spec(),
        crate::anthropic::spec(),
        crate::ollama::spec(),

        // ── OpenAI-compatible providers (table-driven) ────────────────────────
        // International
        crate::openai_compat::spec_for("deepseek"),
        crate::openai_compat::spec_for("groq"),
        crate::openai_compat::spec_for("openrouter"),
        crate::openai_compat::spec_for("gemini"),
        crate::openai_compat::spec_for("mistral"),
        crate::openai_compat::spec_for("xai"),
        // Chinese providers
        crate::openai_compat::spec_for("qwen"),
        crate::openai_compat::spec_for("glm"),
        crate::openai_compat::spec_for("moonshot"),
        crate::openai_compat::spec_for("minimax"),
    ]
}
