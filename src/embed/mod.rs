//! Embedding abstraction. The engine never talks to a concrete provider —
//! everything goes through the [`Embedder`] trait, so the vectorizer is
//! swappable at build/run time:
//!
//! | Backend | Feature | Notes |
//! |---|---|---|
//! | [`local::LocalEmbedder`] | `local` | real sentence embeddings **in-process** (candle + safetensors, CPU) — **default** (`minilm:384`) |
//! | [`hashing::HashingEmbedder`] | always | dependency-free, deterministic, lexical similarity only; offline fallback and great for tests |
//! | [`http::HttpEmbedder`] | `http` | any OpenAI-compatible `/embeddings` endpoint |

pub mod hashing;

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "local")]
pub mod local;

pub use hashing::HashingEmbedder;
#[cfg(feature = "http")]
pub use http::HttpEmbedder;
#[cfg(feature = "local")]
pub use local::{LocalEmbedder, LocalEmbedderConfig};

use crate::error::{MemoryError, Result};
use serde::Deserialize;
#[cfg(any(feature = "local", feature = "http"))]
use std::path::PathBuf;
use std::sync::Arc;

/// Vectorizes text. Implementations must return **L2-normalized** vectors so
/// cosine similarity reduces to a dot product, and must be deterministic for a
/// given text + config so vectors remain comparable across restarts.
pub trait Embedder: Send + Sync {
    /// Short name used in fingerprints (`"hashing"`, `"minilm"`, `"http"`).
    fn name(&self) -> &'static str;
    /// Dimensionality of produced vectors (constant per instance).
    fn dims(&self) -> usize;
    /// Sensible similarity floor for "is this even related" — embedders with
    /// different geometries produce differently distributed similarities.
    fn default_min_similarity(&self) -> f32 {
        0.2
    }
    /// Fingerprint stored alongside persisted vectors; a mismatch means the
    /// store must be re-indexed before it can be searched.
    fn fingerprint(&self) -> String {
        format!("{}:{}", self.name(), self.dims())
    }
    /// Embed a batch of texts. Output order matches input order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Which embedding backend to build. Construct programmatically or via
/// [`EmbedderConfig::from_env`].
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EmbedderConfig {
    Hashing {
        #[serde(default = "default_hashing_dims")]
        dims: usize,
    },
    #[cfg(feature = "local")]
    Local {
        /// Preset id: `minilm` (default) or `bge-small`.
        #[serde(default = "default_preset")]
        model: String,
        /// Load weights/tokenizer from this directory instead of the hub
        /// (expects `config.json`, `tokenizer.json`, `model.safetensors`).
        #[serde(default)]
        dir: Option<PathBuf>,
        /// Where downloaded model files are cached.
        #[serde(default)]
        cache_dir: Option<PathBuf>,
    },
    #[cfg(feature = "http")]
    Http {
        /// Full URL, e.g. `https://api.openai.com/v1/embeddings`.
        url: String,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        model: Option<String>,
        /// Optional; probed from the first response when omitted.
        #[serde(default)]
        dims: Option<usize>,
    },
}

fn default_hashing_dims() -> usize {
    512
}

#[cfg(feature = "local")]
fn default_preset() -> String {
    "minilm".to_string()
}

impl Default for EmbedderConfig {
    /// `minilm:384` — the in-process MiniLM embedder. Builds without the
    /// `local` feature fall back to the dependency-free hashing embedder.
    fn default() -> Self {
        #[cfg(feature = "local")]
        return EmbedderConfig::Local {
            model: default_preset(),
            dir: None,
            cache_dir: None,
        };
        #[cfg(not(feature = "local"))]
        return EmbedderConfig::Hashing {
            dims: default_hashing_dims(),
        };
    }
}

impl EmbedderConfig {
    pub fn build(&self) -> Result<Arc<dyn Embedder>> {
        match self {
            EmbedderConfig::Hashing { dims } => Ok(Arc::new(HashingEmbedder::new(*dims))),
            #[cfg(feature = "local")]
            EmbedderConfig::Local {
                model,
                dir,
                cache_dir,
            } => Ok(Arc::new(LocalEmbedder::load(LocalEmbedderConfig {
                preset: model.clone(),
                dir: dir.clone(),
                cache_dir: cache_dir.clone(),
            })?)),
            #[cfg(feature = "http")]
            EmbedderConfig::Http {
                url,
                api_key,
                model,
                dims,
            } => Ok(Arc::new(HttpEmbedder::new(
                url.clone(),
                api_key.clone(),
                model.clone(),
                *dims,
            )?) as Arc<dyn Embedder>),
        }
    }

    /// Build from environment variables (all optional unless the chosen
    /// backend needs them):
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `TM_EMBEDDER` | `local` \| `hashing` \| `http` (default `local`; `hashing` in builds without the `local` feature) |
    /// | `TM_EMBEDDER_DIMS` | dims for the hashing backend (default 512) |
    /// | `TM_MODEL` | local preset: `minilm` \| `bge-small` (default `minilm`) |
    /// | `TM_MODEL_DIR` | local model directory override |
    /// | `TM_HF_CACHE` | cache dir for hub downloads |
    ///
    /// The `http` backend shares the single OpenAI-compatible provider config
    /// (`credentials.json` / `TM_LLM_*` — see `LlmConfig`); there is no
    /// separate embeddings configuration.
    pub fn from_env() -> Result<Self> {
        let kind = std::env::var("TM_EMBEDDER").unwrap_or_else(|_| {
            if cfg!(feature = "local") {
                "local"
            } else {
                "hashing"
            }
            .into()
        });
        match kind.as_str() {
            "hashing" => Ok(EmbedderConfig::Hashing {
                dims: std::env::var("TM_EMBEDDER_DIMS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(512),
            }),
            #[cfg(feature = "local")]
            "local" => Ok(EmbedderConfig::Local {
                model: std::env::var("TM_MODEL").unwrap_or_else(|_| "minilm".into()),
                dir: std::env::var("TM_MODEL_DIR").ok().map(PathBuf::from),
                cache_dir: std::env::var("TM_HF_CACHE").ok().map(PathBuf::from),
            }),
            #[cfg(not(feature = "local"))]
            "local" => Err(MemoryError::Embedder(
                "TM_EMBEDDER=local but this binary was built without the `local` feature — rebuild with `--features local`".into(),
            )),
            #[cfg(feature = "http")]
            "http" => {
                let data_dir = std::env::var("TM_DATA_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| crate::store::default_data_dir());
                match crate::llm::LlmConfig::resolve(None, &data_dir)? {
                    Some(c) if !c.base_url.trim().is_empty() => Ok(EmbedderConfig::Http {
                        url: c.base_url,
                        api_key: c.api_key,
                        model: (!c.model.trim().is_empty()).then_some(c.model),
                        dims: None, // probed from the first response
                    }),
                    _ => Err(MemoryError::Embedder(
                        "TM_EMBEDDER=http needs the OpenAI-compatible provider config — run `tiered-memory credentials` or set TM_LLM_BASE_URL / TM_LLM_API_KEY / TM_LLM_MODEL".into(),
                    )),
                }
            }
            #[cfg(not(feature = "http"))]
            "http" => Err(MemoryError::Embedder(
                "TM_EMBEDDER=http but this binary was built without the `http` feature".into(),
            )),
            other => Err(MemoryError::Embedder(format!(
                "unknown TM_EMBEDDER `{other}` (hashing | local | http)"
            ))),
        }
    }
}
