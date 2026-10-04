//! Remote embedder for any OpenAI-compatible `/embeddings` endpoint
//! (OpenAI, OpenRouter, Ollama, LM Studio, vLLM, ...). Feature flag: `http`.

use crate::embed::Embedder;
use crate::error::{MemoryError, Result};
use crate::vector::normalize;
use std::sync::OnceLock;

pub struct HttpEmbedder {
    url: String,
    api_key: Option<String>,
    model: Option<String>,
    dims: OnceLock<usize>,
}

impl HttpEmbedder {
    /// Build and probe dimensionality (one tiny request) so fingerprints are
    /// stable before the first real embed.
    pub fn new(
        url: String,
        api_key: Option<String>,
        model: Option<String>,
        dims: Option<usize>,
    ) -> Result<Self> {
        let e = HttpEmbedder {
            url,
            api_key,
            model,
            dims: OnceLock::new(),
        };
        match dims {
            Some(d) => {
                e.dims.set(d).ok().unwrap();
            }
            None => {
                let v = e
                    .embed(&[".".to_string()])?
                    .into_iter()
                    .next()
                    .ok_or_else(|| MemoryError::Embedder("empty probe response".into()))?;
                let _ = e.dims.set(v.len());
            }
        }
        Ok(e)
    }
}

impl Embedder for HttpEmbedder {
    fn name(&self) -> &'static str {
        "http"
    }

    fn dims(&self) -> usize {
        *self.dims.get().unwrap_or(&0)
    }

    fn default_min_similarity(&self) -> f32 {
        0.25
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut req = ureq::post(&self.url);
        if let Some(key) = &self.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let body = serde_json::json!({
            "input": texts,
            "model": self.model,
        });
        let mut resp = req
            .send_json(body)
            .map_err(|e| MemoryError::Embedder(format!("embeddings request failed: {e}")))?;
        let parsed: serde_json::Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| MemoryError::Embedder(format!("bad embeddings response: {e}")))?;
        let data = parsed
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| {
                MemoryError::Embedder(format!("unexpected embeddings response: {parsed}"))
            })?;

        let mut pairs: Vec<(usize, Vec<f32>)> = data
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let emb: Vec<f32> = item
                    .get("embedding")
                    .and_then(|e| e.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_f64())
                            .map(|x| x as f32)
                            .collect()
                    })
                    .ok_or_else(|| {
                        MemoryError::Embedder("missing `embedding` array in response item".into())
                    })?;
                let idx = item
                    .get("index")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(i as u64) as usize;
                Ok((idx, emb))
            })
            .collect::<Result<_>>()?;
        pairs.sort_by_key(|(i, _)| *i);

        if pairs.len() != texts.len() {
            return Err(MemoryError::Embedder(format!(
                "asked for {} embeddings, got {}",
                texts.len(),
                pairs.len()
            )));
        }
        let mut out = Vec::with_capacity(pairs.len());
        for (_, mut v) in pairs {
            if let Some(d) = self.dims.get() {
                if v.len() != *d {
                    return Err(MemoryError::Embedder(format!(
                        "provider returned {} dims, expected {d}",
                        v.len()
                    )));
                }
            } else {
                let _ = self.dims.set(v.len());
            }
            normalize(&mut v);
            out.push(v);
        }
        Ok(out)
    }
}
