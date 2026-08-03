use std::sync::Arc;

use runic_provider::Provider;

use crate::composer::ComposeError;

pub(crate) fn infer(spec: &str) -> Result<(Arc<dyn Provider>, String), ComposeError> {
    let Some((provider_name, model)) = spec.split_once(':') else {
        return Err(ComposeError::InvalidModelSpec {
            spec: spec.to_string(),
        });
    };
    if model.trim().is_empty() {
        return Err(ComposeError::InvalidModelSpec {
            spec: spec.to_string(),
        });
    }
    let provider = build_provider(provider_name)?;
    Ok((provider, model.to_string()))
}

#[cfg(any(feature = "mistral", feature = "anthropic", feature = "gemini"))]
fn api_key(provider: &'static str, env_var: &'static str) -> Result<String, ComposeError> {
    std::env::var(env_var)
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or(ComposeError::MissingApiKey { provider, env_var })
}

pub(crate) fn build_provider(name: &str) -> Result<Arc<dyn Provider>, ComposeError> {
    match name {
        #[cfg(feature = "mistral")]
        "mistral" => Ok(Arc::new(runic_provider::mistral::MistralDriver::new(
            api_key("mistral", "MISTRAL_API_KEY")?,
        ))),
        #[cfg(feature = "anthropic")]
        "anthropic" => Ok(Arc::new(runic_provider::anthropic::AnthropicDriver::new(
            api_key("anthropic", "ANTHROPIC_API_KEY")?,
            "https://api.anthropic.com".to_string(),
        ))),
        #[cfg(feature = "gemini")]
        "gemini" => Ok(Arc::new(runic_provider::gemini::GeminiDriver::new(
            api_key("gemini", "GEMINI_API_KEY")?,
            "https://generativelanguage.googleapis.com".to_string(),
        ))),
        other => Err(ComposeError::UnknownProvider {
            name: other.to_string(),
        }),
    }
}
