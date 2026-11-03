//! OpenAI-compatible chat completions client (Ollama, vLLM, llama.cpp
//! server, LM Studio, OpenAI, ...).

use std::{cell::Cell, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::{config::LlmConfig, mcp::truncate};

/// Something that answers a system + user prompt with text.
pub trait Llm {
    fn complete(&self, system: &str, user: &str) -> Result<String>;
}

pub struct LlmClient {
    http: ureq::Agent,
    url: String,
    config: LlmConfig,
    json_mode: Cell<bool>,
}

impl LlmClient {
    pub fn new(config: &LlmConfig) -> Self {
        let http = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(config.timeout_secs.max(1))))
                .http_status_as_error(false)
                .build(),
        );
        Self {
            http,
            url: format!(
                "{}/chat/completions",
                config.base_url.trim().trim_end_matches('/')
            ),
            json_mode: Cell::new(config.json_mode),
            config: config.clone(),
        }
    }

    fn send(&self, system: &str, user: &str) -> Result<(u16, String)> {
        let mut body = json!({
            "model": self.config.model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
            "stream": false,
        });
        if self.json_mode.get() {
            body["response_format"] = json!({ "type": "json_object" });
        }
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        if !self.config.api_key.trim().is_empty() {
            req = req.header(
                "authorization",
                format!("Bearer {}", self.config.api_key.trim()),
            );
        }
        let mut res = req
            .send(body.to_string())
            .with_context(|| format!("cannot reach the LLM at {}", self.url))?;
        let status = res.status().as_u16();
        let text = res
            .body_mut()
            .with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()
            .context("cannot read the LLM response")?;
        Ok((status, text))
    }
}

impl Llm for LlmClient {
    fn complete(&self, system: &str, user: &str) -> Result<String> {
        let (mut status, mut text) = self.send(system, user)?;
        if (400..500).contains(&status) && status != 401 && status != 403 && self.json_mode.get() {
            tracing::warn!(
                "LLM rejected response_format=json_object (HTTP {status}); retrying without it"
            );
            self.json_mode.set(false);
            (status, text) = self.send(system, user)?;
        }
        if !(200..300).contains(&status) {
            bail!(
                "LLM HTTP {status} from {}: {}",
                self.url,
                truncate(&text, 500)
            );
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("LLM response is not JSON: {}", truncate(&text, 300)))?;
        let choice = &v["choices"][0];
        let content = choice["message"]["content"]
            .as_str()
            .or_else(|| choice["text"].as_str())
            .ok_or_else(|| {
                anyhow!(
                    "LLM response has no message content: {}",
                    truncate(&text, 300)
                )
            })?;
        if choice["finish_reason"] == "length" {
            tracing::warn!(
                "LLM output was cut at max_tokens ({})",
                self.config.max_tokens
            );
        }
        Ok(content.to_string())
    }
}

/// The JSON object in an LLM answer: reasoning (`<think>...</think>`) and
/// code fences are skipped, surrounding prose is ignored.
pub fn extract_json(text: &str) -> Result<Value> {
    let mut text = text.to_string();
    while let (Some(start), Some(end)) = (text.find("<think>"), text.find("</think>")) {
        if end < start {
            break;
        }
        text.replace_range(start..end + "</think>".len(), "");
    }
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            return Ok(v);
        }
    }
    // First '{' that starts a complete JSON object.
    for (i, _) in trimmed.match_indices('{') {
        let mut stream = serde_json::Deserializer::from_str(&trimmed[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = stream.next() {
            if v.is_object() {
                return Ok(v);
            }
        }
    }
    bail!(
        "no JSON object in the LLM answer: {}",
        truncate(trimmed, 400)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_in_noisy_answers() {
        assert_eq!(extract_json("{\"ops\": []}").unwrap()["ops"], json!([]));
        let fenced = "Here you go:\n```json\n{\"ops\": [{\"op\": \"noop\"}]}\n```\nDone.";
        assert_eq!(extract_json(fenced).unwrap()["ops"][0]["op"], "noop");
        let thinking = "<think>maybe {not json}</think>\n{\"verdicts\": []}";
        assert!(extract_json(thinking).unwrap()["verdicts"].is_array());
        assert!(extract_json("no json here").is_err());
        let braces_in_prose = "Use {curly} style. {\"a\": 1}";
        assert_eq!(extract_json(braces_in_prose).unwrap()["a"], 1);
    }
}
