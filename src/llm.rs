//! OpenAI-compatible LLM client for the memory-extraction pipeline
//! (`tiered-memory sync`), the model catalog, and the `http` embedder —
//! **one provider config powers all three**.
//!
//! Credentials resolve in order: `{data}/credentials.json` (written by
//! `tiered-memory credentials`, permissions 0600) > `TM_LLM_BASE_URL` /
//! `TM_LLM_API_KEY` / `TM_LLM_MODEL` env vars. Any OpenAI-compatible provider
//! works — OpenAI, OpenRouter, Groq, Ollama (`http://localhost:11434/v1`),
//! LM Studio, vLLM.

use crate::error::{MemoryError, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CREDENTIALS_FILE: &str = "credentials.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfig {
    /// e.g. `https://api.openai.com/v1`, `http://localhost:11434/v1`
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub temperature: Option<f32>,
}

impl LlmConfig {
    /// Persist to disk with owner-only permissions (the api key lives here).
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self)?;
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            f.write_all(&body)?;
        }
        #[cfg(not(unix))]
        std::fs::write(path, body)?;
        Ok(())
    }

    pub fn load_from(path: &Path) -> Result<Option<LlmConfig>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemoryError::Io(e)),
        }
    }

    /// Resolve credentials: explicit file > default data-dir file > env.
    /// `Ok(None)` means nothing is configured — callers should explain setup.
    pub fn resolve(explicit_path: Option<&Path>, data_dir: &Path) -> Result<Option<LlmConfig>> {
        if let Some(p) = explicit_path {
            return LlmConfig::load_from(p);
        }
        if let Some(c) = LlmConfig::load_from(&data_dir.join(CREDENTIALS_FILE))? {
            return Ok(Some(c));
        }
        let base_url = std::env::var("TM_LLM_BASE_URL").ok();
        let api_key = std::env::var("TM_LLM_API_KEY").ok();
        let model = std::env::var("TM_LLM_MODEL").ok();
        if base_url.is_none() && api_key.is_none() && model.is_none() {
            return Ok(None);
        }
        Ok(Some(LlmConfig {
            base_url: base_url.unwrap_or_default(),
            api_key,
            model: model.unwrap_or_default(),
            temperature: None,
        }))
    }

    fn validate(&self) -> Result<()> {
        if self.base_url.trim().is_empty() || self.model.trim().is_empty() {
            return Err(MemoryError::invalid(
                "credentials need at least base_url and model (tiered-memory credentials set --base-url … --model …)",
            ));
        }
        Ok(())
    }
}

pub struct LlmClient {
    config: LlmConfig,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<String>,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Result<Self> {
        config.validate()?;
        Ok(LlmClient { config })
    }

    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// One-shot chat completion: system prompt + user payload → text.
    pub fn chat(&self, system: &str, user: &str) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut req = ureq::post(&url)
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(120)))
            .build();
        if let Some(key) = &self.config.api_key {
            if !key.trim().is_empty() {
                req = req.header("Authorization", &format!("Bearer {}", key.trim()));
            }
        }
        let body = serde_json::json!({
            "model": self.config.model,
            "temperature": self.config.temperature.unwrap_or(0.2),
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
        });
        let mut resp = req
            .send_json(body)
            .map_err(|e| MemoryError::Embedder(format!("LLM request to {url} failed: {e}")))?;
        let parsed: ChatResponse = resp
            .body_mut()
            .read_json()
            .map_err(|e| MemoryError::Embedder(format!("bad LLM response: {e}")))?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| MemoryError::Embedder("LLM returned no choices".into()))
    }
}

/// Extract the first JSON object from an LLM reply (handles ```json fences
/// and surrounding prose).
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    let fenced = trimmed
        .find("```")
        .and_then(|start| {
            let after = &trimmed[start..];
            let nl = after.find('\n')?;
            let body = &after[nl + 1..];
            let end = body.find("```")?;
            Some(body[..end].trim())
        })
        .unwrap_or(trimmed);
    let candidate = fenced.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
        if v.is_object() {
            return Some(v);
        }
    }
    // fall back to the outermost braces in the text
    let start = candidate.find('{')?;
    let end = candidate.rfind('}')?;
    if end > start {
        serde_json::from_str::<serde_json::Value>(&candidate[start..=end]).ok()
    } else {
        None
    }
}

/// Mask an API key for display: `sk-t…90`.
pub fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "*".repeat(key.len());
    }
    format!("{}…{}", &key[..4], &key[key.len() - 2..])
}

/// Fetch the model catalog from an OpenAI-compatible provider
/// (`GET {base_url}/models`). Used by the credentials wizard's searchable
/// model picker and the `tiered-memory models` command.
pub fn fetch_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = ureq::get(&url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .build();
    if let Some(key) = api_key {
        if !key.trim().is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", key.trim()));
        }
    }
    let mut resp = req
        .call()
        .map_err(|e| MemoryError::Embedder(format!("model list request to {url} failed: {e}")))?;
    let parsed: serde_json::Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| MemoryError::Embedder(format!("bad model list response: {e}")))?;
    let models = parse_models_json(&parsed);
    if models.is_empty() {
        return Err(MemoryError::Embedder(format!(
            "no models found in the response from {url}"
        )));
    }
    Ok(models)
}

/// Extract model ids from a `/models` response. Handles the OpenAI shape
/// (`{"data": [{"id": …}]}`) plus common variants (`{"models": […]}`, bare
/// string arrays) so local servers that bend the spec still work.
pub fn parse_models_json(v: &serde_json::Value) -> Vec<String> {
    let mut ids = Vec::new();
    let mut collect = |item: &serde_json::Value| {
        if let Some(s) = item.as_str() {
            ids.push(s.to_string());
        } else if let Some(s) = item.get("id").and_then(|x| x.as_str()) {
            ids.push(s.to_string());
        } else if let Some(s) = item.get("name").and_then(|x| x.as_str()) {
            ids.push(s.to_string());
        }
    };
    for key in ["data", "models"] {
        if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
            for item in arr {
                collect(item);
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_handles_fences_and_prose() {
        let v = extract_json("```json\n{\"updates\": []}\n```").unwrap();
        assert!(v["updates"].is_array());
        let v = extract_json("Sure! Here it is: {\"updates\": [{\"level\":\"L1\"}]} done").unwrap();
        assert_eq!(v["updates"][0]["level"], "L1");
        assert!(extract_json("no json here").is_none());
    }

    #[test]
    fn parse_models_handles_openai_and_variants() {
        let openai = json!({ "data": [ {"id": "gpt-4o-mini"}, {"id": "gpt-4o"} ] });
        assert_eq!(parse_models_json(&openai), vec!["gpt-4o", "gpt-4o-mini"]);
        let alt = json!({ "models": [ "llama3.2", {"name": "qwen2.5"} ] });
        assert_eq!(parse_models_json(&alt), vec!["llama3.2", "qwen2.5"]);
        assert!(parse_models_json(&json!({})).is_empty());
    }

    #[test]
    fn credentials_resolve_from_env() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TM_LLM_BASE_URL", "http://localhost:9/v1");
        std::env::set_var("TM_LLM_MODEL", "test-model");
        std::env::remove_var("TM_LLM_API_KEY");
        let cfg = LlmConfig::resolve(None, dir.path()).unwrap().unwrap();
        assert_eq!(cfg.model, "test-model");
        std::env::remove_var("TM_LLM_BASE_URL");
        std::env::remove_var("TM_LLM_MODEL");
        assert!(LlmConfig::resolve(None, dir.path()).unwrap().is_none());
    }
}
