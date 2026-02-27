use adaclaw_core::provider::{Provider, ProviderCapabilities};

pub struct ProviderSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub local: bool,
    pub capabilities: ProviderCapabilities,
    pub factory: fn(key: Option<&str>, url: Option<&str>) -> Box<dyn Provider>,
}

pub static PROVIDER_REGISTRY: &[ProviderSpec] = &[
    crate::openai::SPEC,
    crate::anthropic::SPEC,
    crate::ollama::SPEC,
    crate::openrouter::SPEC,
    crate::deepseek::SPEC,
];
