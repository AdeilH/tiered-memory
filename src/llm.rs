//! OpenAI-compatible LLM client for the memory-extraction pipeline
//! (`tiered-memory sync`) and credential management.
//!
//! Credentials resolve in order: explicit `--credentials <file>` >
//! `{data}/credentials.json` (written by `tiered-memory credentials set`,
//! permissions 0600) > `TM_LLM_BASE_URL` / `TM_LLM_API_KEY` / `TM_LLM_MODEL`
//! env vars. Any OpenAI-compatible provider works — OpenAI, OpenRouter,
//! Groq, Ollama (`http://localhost:11434/v1`), LM Studio, vLLM.

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
        let mut req = ureq::post(&url).timeout(std::time::Duration::from_secs(120));
        if let Some(key) = &self.config.api_key {
            if !key.trim().is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", key.trim()));
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
        let resp = req
            .send_json(body)
            .map_err(|e| MemoryError::Embedder(format!("LLM request to {url} failed: {e}")))?;
        let parsed: ChatResponse = resp
            .into_json()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_handles_fences_and_prose() {
        let v = extract_json("```json\n{\"updates\": []}\n```").unwrap();
        assert!(v["updates"].is_array());
        let v = extract_json("Sure! Here it is: {\"updates\": [{\"level\":\"L1\"}]} done").unwrap();
        assert_eq!(v["updates"][0]["level"], "L1");
        assert!(extract_json("no json here").is_none());
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
