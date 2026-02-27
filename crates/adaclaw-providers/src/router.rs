use crate::registry::PROVIDER_REGISTRY;
use adaclaw_core::provider::Provider;
use anyhow::{anyhow, Result};

pub fn create_provider(name_or_alias: &str, key: Option<&str>, url: Option<&str>) -> Result<Box<dyn Provider>> {
    for spec in PROVIDER_REGISTRY {
        if spec.name == name_or_alias || spec.aliases.contains(&name_or_alias) {
            return Ok((spec.factory)(key, url));
        }
    }
    Err(anyhow!("Provider not found for: {}", name_or_alias))
}
