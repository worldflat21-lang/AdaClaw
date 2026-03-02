use crate::registry::build_registry;
use adaclaw_core::provider::Provider;
use anyhow::{Result, anyhow};

/// Create a provider instance by name or alias.
///
/// `name_or_alias` is matched against each spec's `name` field first, then
/// its `aliases` slice.  The registry is built fresh on each call (it is
/// cheap — all specs are static data), so there is no global mutable state.
pub fn create_provider(
    name_or_alias: &str,
    key: Option<&str>,
    url: Option<&str>,
) -> Result<Box<dyn Provider>> {
    for spec in build_registry() {
        if spec.name == name_or_alias || spec.aliases.contains(&name_or_alias) {
            return Ok((spec.factory)(key, url));
        }
    }
    Err(anyhow!(
        "Provider not found for: '{}'.  Run `adaclaw providers` to list available providers.",
        name_or_alias
    ))
}

/// Return the names of all registered providers.
pub fn list_providers() -> Vec<&'static str> {
    build_registry().into_iter().map(|s| s.name).collect()
}
