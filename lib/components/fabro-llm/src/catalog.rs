//! Catalog construction and the agent-profile reading that is Fabro's own.
//!
//! Layer order is fixed: lithos built-ins, then the operator's `[llm]`
//! overlay. Which providers are on, which model a selector names, and which
//! model to pick for a job are lithos questions, answered by
//! [`Catalog`] and [`CatalogProvider`] (`enabled_providers`,
//! `offerings_matching`, `default_offering_for`, and the rest). What stays
//! here is the coding harness a model expects, read from the shared
//! `metadata.agent` namespace that Pebble reads too.

use fabro_config::LlmLayer;
use fabro_static::EnvVars;
use fabro_types::AgentProfileKind;
pub use lithos_llm::catalog::Offering;
use lithos_llm::catalog::{
    Catalog, CatalogBuilder, CatalogError, CatalogModel, CatalogProvider, CodecId, Metadata,
    adapter_ids, codec_ids,
};
use serde::Deserialize;

/// The metadata namespace agent harnesses read.
const AGENT_METADATA_NAMESPACE: &str = "agent";

/// Builds the effective catalog.
///
/// `env_lookup` supplies the environment overrides Fabro honors, so test
/// doubles and gateways can stand in for the real APIs without editing
/// settings: `OPENAI_BASE_URL` repoints the `openai` provider, and
/// `ANTHROPIC_BASE_URL` the `anthropic` one. `ANTHROPIC_MODEL` adds that wire
/// model to `anthropic` as its default, small-default and probe model, for a
/// gateway that serves its own model names.
pub fn build_catalog(
    overlay: &LlmLayer,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Catalog, CatalogError> {
    let mut builder = Catalog::builder().with_builtin();
    if !overlay.is_empty() {
        let mut document = overlay.to_overlay_toml();
        document.insert_str(0, "schema_version = 1\n");
        builder = builder.toml_layer("settings [llm]", &document)?;
    }
    for (name, document) in env_layers(env_lookup) {
        builder = builder.toml_layer(name, &document)?;
    }
    let catalog = builder.build()?;
    // The coding agent reads a provider's harness profile from
    // `metadata.agent.profile` and refuses a provider without one. The lithos
    // built-ins all declare theirs; an operator-defined provider that does
    // not gets the profile its wire protocol implies, layered on last.
    let implied = implied_agent_profiles(&catalog);
    if implied.is_empty() {
        return Ok(catalog);
    }
    let mut builder = Catalog::builder().with_builtin();
    if !overlay.is_empty() {
        let mut document = overlay.to_overlay_toml();
        document.insert_str(0, "schema_version = 1\n");
        builder = builder.toml_layer("settings [llm]", &document)?;
    }
    for (name, document) in env_layers(env_lookup) {
        builder = builder.toml_layer(name, &document)?;
    }
    builder
        .toml_layer("implied agent profiles", &implied)
        .and_then(CatalogBuilder::build)
}

/// The catalog layers the environment overrides imply, in application order.
fn env_layers(env_lookup: &dyn Fn(&str) -> Option<String>) -> Vec<(&'static str, String)> {
    let mut layers = Vec::new();
    for (name, provider) in [
        (EnvVars::OPENAI_BASE_URL, "openai"),
        (EnvVars::ANTHROPIC_BASE_URL, "anthropic"),
    ] {
        if let Some(base_url) = env_lookup(name).filter(|value| !value.trim().is_empty()) {
            layers.push((
                name,
                format!(
                    "schema_version = 1\n[providers.{provider}]\nbase_url = {}\n",
                    toml::Value::String(base_url.trim().trim_end_matches('/').trim_end_matches("/v1").to_string())
                ),
            ));
        }
    }
    if let Some(api_model) = env_lookup(EnvVars::ANTHROPIC_MODEL)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        layers.push((EnvVars::ANTHROPIC_MODEL, anthropic_model_layer(&api_model)));
    }
    layers
}

/// Adds `api_model` to the `anthropic` provider as its default, small-default
/// and probe model, taking those roles from the built-in rows. The catalog id
/// is the part after the last `/`, so a gateway route like
/// `cc/claude-haiku-5-5` never reads as a `provider/model` selector.
fn anthropic_model_layer(api_model: &str) -> String {
    use std::fmt::Write as _;

    let id = api_model.rsplit('/').next().unwrap_or(api_model);
    let quoted_id = toml::Value::String(id.to_string());
    let mut document = format!(
        "schema_version = 1\n[providers.anthropic]\ndefault_model = {quoted_id}\n\n"
    );
    for builtin in BUILTIN_ANTHROPIC_ROLE_MODELS {
        if *builtin != id {
            let _ = writeln!(
                document,
                "[providers.anthropic.models.{}]\nsmall_default = false\nprobe = false\n",
                toml::Value::String((*builtin).to_string())
            );
        }
    }
    let _ = writeln!(
        document,
        "[providers.anthropic.models.{quoted_id}]\ndisplay_name = {quoted_id}\napi_model = {}\n\
         limits = {{ context_tokens = 200000, max_output_tokens = 64000 }}\n\
         capabilities = {{ text = true, images = true, documents = true, tools = true, caching = true }}\n\
         small_default = true\nprobe = true",
        toml::Value::String(api_model.to_string())
    );
    document
}

/// The built-in `anthropic` rows that hold the `small_default` or `probe`
/// role, which an `ANTHROPIC_MODEL` override takes over.
const BUILTIN_ANTHROPIC_ROLE_MODELS: &[&str] = &["claude-haiku-4.5"];

/// A TOML layer declaring `metadata.agent.profile` for every provider that
/// has none, as the profile implied by the provider's adapter. Empty when
/// every provider already says.
fn implied_agent_profiles(catalog: &Catalog) -> String {
    use std::fmt::Write as _;

    let mut document = String::new();
    for provider in catalog.providers() {
        if agent_metadata(provider.metadata()).profile.is_some() {
            continue;
        }
        let profile = adapter_agent_profile(provider);
        let _ = writeln!(
            document,
            "[providers.{}.metadata.agent]\nprofile = {}\n",
            provider.id(),
            toml::Value::String(
                serde_json::to_value(profile)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "openai".to_string())
            ),
        );
    }
    if document.is_empty() {
        return document;
    }
    document.insert_str(0, "schema_version = 1\n");
    document
}

/// The profile a provider's wire protocol implies, for a provider whose
/// catalog entry does not name one. The protocol is the provider's first
/// codec, the one the client sends a generation call on; the `bedrock`
/// adapter speaks Converse to Anthropic-shaped models.
fn adapter_agent_profile(provider: &CatalogProvider) -> AgentProfileKind {
    if provider.adapter().as_str() == adapter_ids::BEDROCK {
        return AgentProfileKind::Anthropic;
    }
    match provider.codecs().first().map(CodecId::as_str) {
        Some(codec_ids::ANTHROPIC_MESSAGES) => AgentProfileKind::Anthropic,
        Some(codec_ids::GEMINI_GENERATE) => AgentProfileKind::Gemini,
        _ => AgentProfileKind::OpenAi,
    }
}

/// The catalog with no operator overlay: the lithos built-ins.
///
/// Used where no settings file is in play, such as the standalone hook
/// runner. Servers and the CLI build from the operator's `[llm]` overlay
/// with [`build_catalog`] instead.
#[must_use]
pub fn default_catalog() -> Catalog {
    build_catalog(&LlmLayer::default(), &|_| None).expect("the built-in catalog always builds")
}

/// The `metadata.agent` namespace on a catalog entry. Malformed metadata
/// falls back to the defaults; the lithos built-ins are validated in lithos.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentMetadata {
    profile:              Option<AgentProfileKind>,
    reasoning_by_default: Option<bool>,
}

fn agent_metadata(metadata: &Metadata) -> AgentMetadata {
    metadata
        .namespace::<AgentMetadata>(AGENT_METADATA_NAMESPACE)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Whether requests to `offering` reason when no effort is requested.
///
/// The catalog can state it outright under `metadata.agent`. Otherwise a
/// model that supports reasoning and takes named effort levels reasons by
/// default, while one that needs an explicit thinking budget does not.
#[must_use]
pub fn reasons_by_default(offering: &Offering<'_>) -> bool {
    agent_metadata(offering.model.metadata())
        .reasoning_by_default
        .or(agent_metadata(offering.provider.metadata()).reasoning_by_default)
        .unwrap_or_else(|| {
            offering.model.capabilities().reasoning().is_supported()
                && offering.model.protocol_options().reasoning_effort_levels
        })
}

fn model_agent_profile(provider: &CatalogProvider, model: &CatalogModel) -> AgentProfileKind {
    agent_metadata(model.metadata())
        .profile
        .unwrap_or_else(|| provider_agent_profile(provider))
}

/// The agent profile a provider's models run under unless a model row says
/// otherwise: the provider's `metadata.agent.profile`, which
/// [`build_catalog`] fills in for a provider that declared none.
fn provider_agent_profile(provider: &CatalogProvider) -> AgentProfileKind {
    agent_metadata(provider.metadata())
        .profile
        .unwrap_or_else(|| adapter_agent_profile(provider))
}

/// The agent profile for a route on an enabled provider. Unknown
/// (passthrough) models take the provider default; a disabled or unknown
/// provider has none.
#[must_use]
pub fn agent_profile(
    catalog: &Catalog,
    provider_selector: &str,
    model_selector: Option<&str>,
) -> Option<AgentProfileKind> {
    let provider = catalog.enabled_provider(provider_selector)?;
    Some(
        match model_selector.and_then(|selector| provider.model(selector)) {
            Some(model) => model_agent_profile(provider, model),
            None => provider_agent_profile(provider),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_catalog;

    #[test]
    fn operator_overlay_applies_last() {
        let overlay = LlmLayer(
            toml::from_str(
                r"
[providers.openai]
priority = 500
enabled = false
",
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        assert!(catalog.enabled_provider("openai").is_none());
        assert_eq!(
            catalog.provider("openai").unwrap().priority(),
            500,
            "overlay values win over the built-ins"
        );
    }

    #[test]
    fn openai_base_url_env_repoints_the_openai_provider() {
        let catalog = build_catalog(&LlmLayer::default(), &|name| {
            (name == EnvVars::OPENAI_BASE_URL).then(|| "http://127.0.0.1:1234/v1".to_string())
        })
        .unwrap();
        assert_eq!(
            catalog.provider("openai").unwrap().base_url(),
            "http://127.0.0.1:1234"
        );
    }

    #[test]
    fn anthropic_env_repoints_the_provider_and_sets_its_model() {
        let catalog = build_catalog(&LlmLayer::default(), &|name| match name {
            EnvVars::ANTHROPIC_BASE_URL => Some("http://gateway.test:20128/".to_string()),
            EnvVars::ANTHROPIC_MODEL => Some("cc/claude-haiku-5-5".to_string()),
            _ => None,
        })
        .unwrap();
        let anthropic = catalog.enabled_provider("anthropic").unwrap();
        assert_eq!(anthropic.base_url(), "http://gateway.test:20128");
        let default = anthropic.default_offering().unwrap();
        assert_eq!(default.model.id().as_str(), "claude-haiku-5-5");
        assert_eq!(default.model.api_model(), "cc/claude-haiku-5-5");
        assert_eq!(
            anthropic.probe_offering().unwrap().model.id().as_str(),
            "claude-haiku-5-5"
        );
        assert!(!anthropic.model("claude-haiku-4.5").unwrap().is_probe());
        assert_eq!(
            agent_profile(&catalog, "anthropic", Some("claude-haiku-5-5")),
            Some(AgentProfileKind::Anthropic)
        );
    }

    #[test]
    fn blank_anthropic_env_changes_nothing() {
        let catalog = build_catalog(&LlmLayer::default(), &|name| match name {
            EnvVars::ANTHROPIC_BASE_URL | EnvVars::ANTHROPIC_MODEL => Some("  ".to_string()),
            _ => None,
        })
        .unwrap();
        let anthropic = catalog.enabled_provider("anthropic").unwrap();
        assert_eq!(anthropic.base_url(), "https://api.anthropic.com");
        assert!(anthropic.model("claude-haiku-4.5").unwrap().is_probe());
    }

    #[test]
    fn agent_profiles_follow_the_model_then_the_provider() {
        let catalog = test_catalog();
        assert_eq!(
            agent_profile(&catalog, "openai", Some("gpt-5.6-sol")),
            Some(AgentProfileKind::Gpt56)
        );
        assert_eq!(
            agent_profile(&catalog, "moonshot", None),
            Some(AgentProfileKind::Kimi),
            "a passthrough model on Moonshot takes the provider's Kimi profile"
        );
        assert_eq!(
            agent_profile(&catalog, "deepseek", None),
            Some(AgentProfileKind::OpenAi)
        );
        assert_eq!(
            agent_profile(&catalog, "bedrock-openai", None),
            None,
            "disabled providers have no profile to offer"
        );
        assert_eq!(
            agent_profile(&catalog, "moonshot", Some("kimi-k3")),
            Some(AgentProfileKind::Kimi)
        );
        assert_eq!(
            agent_profile(&catalog, "openai", Some("gpt-6-astra")),
            Some(AgentProfileKind::Gpt6)
        );
        assert_eq!(
            agent_profile(&catalog, "anthropic", Some("claude-sonnet-4.5")),
            Some(AgentProfileKind::Anthropic)
        );
    }

    #[test]
    fn reasoning_by_default_reads_agent_metadata_then_capabilities() {
        let catalog = test_catalog();
        let moonshot = catalog.enabled_provider("moonshot").unwrap();
        let kimi = moonshot.offering("kimi-k2.5").unwrap();
        assert!(reasons_by_default(&kimi), "the catalog row says so");
        let anthropic = catalog.enabled_provider("anthropic").unwrap();
        let sonnet = anthropic.offering("claude-sonnet-4.5").unwrap();
        assert!(
            !reasons_by_default(&sonnet),
            "a thinking-budget model reasons only when asked"
        );
    }

    /// An operator-defined provider with no `metadata.agent.profile` gets the
    /// one its adapter implies, so the coding agent can build on it.
    #[test]
    fn operator_providers_without_a_profile_get_the_adapter_implied_one() {
        let overlay = LlmLayer(
            toml::from_str(
                r#"
[providers.acme]
display_name = "Acme"
base_url = "https://api.acme.test/v1"
auth = { type = "bearer" }
default_model = "acme-llama"

[providers.acme.models.acme-llama]
display_name = "Acme Llama"
api_model = "acme-llama"
limits = { context_tokens = 131072, max_output_tokens = 8192 }
capabilities = { text = true, tools = true }
"#,
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        let acme = catalog.provider("acme").unwrap();
        assert_eq!(
            agent_metadata(acme.metadata()).profile,
            Some(AgentProfileKind::OpenAi)
        );
        assert_eq!(
            agent_profile(&catalog, "acme", Some("acme-llama")),
            Some(AgentProfileKind::OpenAi)
        );
    }

    /// The implied profile follows the provider's first codec, so a host that
    /// speaks Anthropic Messages gets the Anthropic harness.
    #[test]
    fn the_implied_profile_follows_the_first_codec() {
        let overlay = LlmLayer(
            toml::from_str(
                r#"
[providers.acme]
display_name = "Acme"
codecs = ["anthropic-messages"]
base_url = "https://api.acme.test"
auth = { type = "header", name = "x-api-key" }
"#,
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        assert_eq!(
            agent_profile(&catalog, "acme", None),
            Some(AgentProfileKind::Anthropic)
        );
    }
}
