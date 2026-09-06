//! Models subcommand: list available models from model spec metadata.
//!
//! Reads configured providers from `~/.anureo/config.toml`, resolves models from
//! model spec, and displays available models. Supports filtering by provider name.
//!
//! **Interaction**: Called from the `anureo` binary when the user runs `anureo models list`
//! or `anureo models show <PROVIDER>`.

use agent::run::RunError;
use config::{load_full_config, ProviderDef};
use anureo_llm::ModelInfo;
use anureo_llm::ProviderConfig;
use model_spec_core::ModelRegistry;
use std::collections::HashMap;

/// Result of querying models from a single provider.
struct ProviderModels {
    provider: String,
    models: Vec<ModelInfo>,
    error: Option<String>,
}

impl ProviderModels {
    fn ok(provider: String, models: Vec<ModelInfo>) -> Self {
        Self {
            provider,
            models,
            error: None,
        }
    }

    fn err(provider: String, error: String) -> Self {
        Self {
            provider,
            models: Vec::new(),
            error: Some(error),
        }
    }
}

/// List models from all configured providers.
pub async fn list_all_models(json: bool) -> Result<(), RunError> {
    let config = load_full_config("anureo")
        .map_err(|e| RunError::ConfigError(format!("Failed to load config: {}", e)))?;

    if config.providers.is_empty() {
        eprintln!("No providers configured in ~/.anureo/config.toml");
        eprintln!("Add a [[providers]] section to your config file.");
        return Ok(());
    }

    let results = query_providers_models_from_spec(&config.providers).await;

    if json {
        output_json(&results);
    } else {
        output_human(&results);
    }

    Ok(())
}

/// List models from a specific provider.
pub async fn list_provider_models(provider_name: &str, json: bool) -> Result<(), RunError> {
    let config = load_full_config("anureo")
        .map_err(|e| RunError::ConfigError(format!("Failed to load config: {}", e)))?;

    let provider = config
        .providers
        .iter()
        .find(|p| p.name == provider_name)
        .ok_or_else(|| RunError::ConfigError(format!("Provider '{}' not found", provider_name)))?;

    let result = query_providers_models_from_spec(std::slice::from_ref(provider))
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| ProviderModels::ok(provider.name.clone(), Vec::new()));

    if json {
        output_json(&[result]);
    } else {
        output_human(&[result]);
    }

    Ok(())
}

/// Query models from providers using model spec.
async fn query_providers_models_from_spec(providers: &[ProviderDef]) -> Vec<ProviderModels> {
    let registry = ModelRegistry::global();
    let provider_configs: Vec<ProviderConfig> = providers
        .iter()
        .map(|p| ProviderConfig {
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            api_key: p.api_key.clone(),
            provider_type: p.provider_type.clone(),
            fetch_models: p.fetch_models.unwrap_or(false),
            cache_ttl: None,
            enable_tier_resolution: true,
            declared_models: p.models.clone().into_iter().map(|m| m.id).collect(),
        })
        .collect();

    match registry.list_all_models_result(&provider_configs).await {
        Ok(entries) => {
            let mut by_provider: HashMap<String, Vec<ModelInfo>> = HashMap::new();
            for entry in entries {
                by_provider
                    .entry(entry.provider)
                    .or_default()
                    .push(ModelInfo {
                        id: entry.name,
                        created: None,
                        owned_by: None,
                    });
            }

            providers
                .iter()
                .map(|provider| {
                    let mut models = by_provider.remove(&provider.name).unwrap_or_default();
                    models.sort_by_key(|a| a.id.clone());
                    ProviderModels::ok(provider.name.clone(), models)
                })
                .collect()
        }
        Err(e) => providers
            .iter()
            .map(|provider| {
                ProviderModels::err(
                    provider.name.clone(),
                    format!("Failed to resolve models from model spec: {e}"),
                )
            })
            .collect(),
    }
}

/// Output results as JSON.
fn output_json(results: &[ProviderModels]) {
    let json_results: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            let models: Vec<serde_json::Value> = r
                .models
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "created": m.created,
                        "owned_by": m.owned_by
                    })
                })
                .collect();
            serde_json::json!({
                "provider": r.provider,
                "models": models,
                "error": r.error
            })
        })
        .collect();

    println!(
        "{}",
        serde_json::to_string_pretty(&json_results).unwrap_or_default()
    );
}

/// Output results in human-readable format.
fn output_human(results: &[ProviderModels]) {
    for result in results {
        println!("\n📦 Provider: {}", result.provider);
        println!("{}", "─".repeat(50));

        if let Some(ref error) = result.error {
            println!("  ❌ Error: {}", error);
            continue;
        }

        if result.models.is_empty() {
            println!("  No models available");
            continue;
        }

        println!("  Models ({}):", result.models.len());
        for model in &result.models {
            println!("  • {}", model.id);
            if let Some(ref owned_by) = model.owned_by {
                println!("    Owner: {}", owned_by);
            }
        }
    }
}
