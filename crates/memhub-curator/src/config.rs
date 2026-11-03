//! Curator configuration (`config/memhub2-curator.yaml`).

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The commented example written by `memhub2 init-config`.
pub const EXAMPLE: &str = include_str!("../../../config/memhub2-curator.example.yaml");

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub hub: HubConfig,
    #[serde(default)]
    pub curation: CurationConfig,
    #[serde(default)]
    pub port: PortConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub json_mode: bool,
    pub max_prompt_chars: usize,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: String::new(),
            api_key: String::new(),
            temperature: 0.1,
            max_tokens: 4096,
            timeout_secs: 300,
            json_mode: true,
            max_prompt_chars: 60_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HubConfig {
    pub url: String,
    pub curator_token: String,
    pub agent: String,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:7880/mcp".into(),
            curator_token: String::new(),
            agent: "memhub-curator".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewMode {
    Llm,
    Auto,
    Human,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CurationConfig {
    pub batch_size: usize,
    pub max_batches_per_target: usize,
    pub review: ReviewMode,
    pub backfill: bool,
    pub backfill_batch_size: usize,
    pub targets: Vec<String>,
}

impl Default for CurationConfig {
    fn default() -> Self {
        Self {
            batch_size: 10,
            max_batches_per_target: 20,
            review: ReviewMode::Llm,
            backfill: true,
            backfill_batch_size: 5,
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PortConfig {
    pub v1_url: String,
    pub v1_agent: String,
}

impl Default for PortConfig {
    fn default() -> Self {
        Self {
            v1_url: "http://127.0.0.1:7878/mcp".into(),
            v1_agent: "memhub-selftest".into(),
        }
    }
}

impl Config {
    /// Reads `path` (if it exists) and applies environment overrides.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut config = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("cannot read {}", p.display()))?;
                serde_yaml::from_str(&text)
                    .with_context(|| format!("invalid curator config {}", p.display()))?
            }
            Some(p) => {
                tracing::warn!(
                    "curator config {} not found; using defaults and environment (create it with `memhub2 init-config`)",
                    p.display()
                );
                Config::default()
            }
            None => Config::default(),
        };
        config.apply_env(|k| std::env::var(k).ok());
        Ok(config)
    }

    pub fn apply_env(&mut self, var: impl Fn(&str) -> Option<String>) {
        let set = |target: &mut String, key: &str| {
            if let Some(v) = var(key).filter(|v| !v.trim().is_empty()) {
                *target = v.trim().to_string();
            }
        };
        set(&mut self.llm.base_url, "MEMHUB2_LLM_BASE_URL");
        set(&mut self.llm.model, "MEMHUB2_LLM_MODEL");
        set(&mut self.llm.api_key, "MEMHUB2_LLM_API_KEY");
        set(&mut self.hub.url, "MEMHUB2_URL");
        set(&mut self.hub.curator_token, "MEMHUB2_CURATOR_TOKEN");
        set(&mut self.port.v1_url, "MEMHUB1_URL");
    }

    /// Checks what `memhub2 curate` needs (the LLM and the curator token).
    pub fn validate_for_curation(&self) -> Result<()> {
        self.validate_hub()?;
        let llm = &self.llm;
        if llm.base_url.trim().is_empty() {
            bail!("llm.base_url is not set (config file or MEMHUB2_LLM_BASE_URL)");
        }
        if llm.model.trim().is_empty() || llm.model == "your-model-name" {
            bail!("llm.model is not set (config file or MEMHUB2_LLM_MODEL)");
        }
        if self.curation.batch_size == 0 {
            bail!("curation.batch_size must be at least 1");
        }
        Ok(())
    }

    /// Checks what talking to the 2.0 hub as curator needs.
    pub fn validate_hub(&self) -> Result<()> {
        if self.hub.url.trim().is_empty() {
            bail!("hub.url is not set (config file or MEMHUB2_URL)");
        }
        if self.hub.curator_token.trim().is_empty() {
            bail!(
                "hub.curator_token is not set (config file or MEMHUB2_CURATOR_TOKEN); it must match \
                 the 2.0 server's --curator-token"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_and_env_wins() {
        let mut config: Config = serde_yaml::from_str(EXAMPLE).unwrap();
        assert_eq!(config.hub.url, "http://127.0.0.1:8020/mcp");
        assert_eq!(config.port.v1_url, "http://127.0.0.1:8010/mcp");
        assert_eq!(config.curation.review, ReviewMode::Llm);
        assert!(
            config.validate_for_curation().is_err(),
            "example has no model or token"
        );
        config.apply_env(|k| match k {
            "MEMHUB2_LLM_MODEL" => Some("qwen".into()),
            "MEMHUB2_CURATOR_TOKEN" => Some(" secret ".into()),
            "MEMHUB2_LLM_API_KEY" => Some("   ".into()),
            _ => None,
        });
        assert_eq!(config.llm.model, "qwen");
        assert_eq!(config.hub.curator_token, "secret");
        assert_eq!(config.llm.api_key, "", "blank env values are ignored");
        config.validate_for_curation().unwrap();
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_yaml::from_str::<Config>("llm:\n  modle: x\n").unwrap_err();
        assert!(err.to_string().contains("modle"), "{err}");
        let partial: Config = serde_yaml::from_str("llm:\n  model: x\n").unwrap();
        assert_eq!(partial.llm.max_tokens, 4096);
    }
}
