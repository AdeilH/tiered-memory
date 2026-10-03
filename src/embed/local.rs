//! Embedded local embedder: a sentence-transformer (BERT-family) running
//! **in-process** on CPU via [candle](https://github.com/huggingface/candle) —
//! no Python, no ONNX runtime, no server. Weights are plain safetensors,
//! either loaded from a local directory or fetched once from the Hugging Face
//! hub and cached.
//!
//! Presets:
//! * `minilm`    — `sentence-transformers/all-MiniLM-L6-v2`, mean pooling, 384 dims (~90 MB)
//! * `bge-small` — `BAAI/bge-small-en-v1.5`, CLS pooling, 384 dims
//!
//! Feature flag: `local`.

use crate::embed::Embedder;
use crate::error::{MemoryError, Result};
use crate::vector::normalize;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use std::io::Read;
use std::path::{Path, PathBuf};
use tokenizers::Tokenizer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pooling {
    /// Sentence-transformers MiniLM style: average token embeddings under the mask.
    Mean,
    /// BGE style: take the [CLS] token embedding.
    Cls,
}

/// (preset id, hf repo, pooling, dims)
pub const PRESETS: &[(&str, &str, Pooling, usize)] = &[
    (
        "minilm",
        "sentence-transformers/all-MiniLM-L6-v2",
        Pooling::Mean,
        384,
    ),
    ("bge-small", "BAAI/bge-small-en-v1.5", Pooling::Cls, 384),
];

#[derive(Debug, Clone)]
pub struct LocalEmbedderConfig {
    pub preset: String,
    pub dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

pub struct LocalEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    pooling: Pooling,
    name: &'static str,
    dims: usize,
    max_len: usize,
    batch_size: usize,
}

struct ModelFiles {
    config: PathBuf,
    tokenizer: PathBuf,
    weights: PathBuf,
}

impl LocalEmbedder {
    pub fn load(cfg: LocalEmbedderConfig) -> Result<Self> {
        let (name, repo_id, pooling, preset_dims) = PRESETS
            .iter()
            .find(|(id, ..)| *id == cfg.preset.as_str())
            .map(|(id, repo, p, d)| (*id, *repo, *p, *d))
            .unwrap_or(("local-bert", cfg.preset.as_str(), Pooling::Mean, 0));

        let files = match &cfg.dir {
            Some(dir) => ModelFiles {
                config: dir.join("config.json"),
                tokenizer: dir.join("tokenizer.json"),
                weights: dir.join("model.safetensors"),
            },
            None => fetch_from_hub(repo_id, cfg.cache_dir.as_deref())?,
        };

        let device = Device::Cpu;
        let config_raw = read_json(&files.config)?;
        let bert_config = bert_config_from_json(&config_raw)
            .map_err(|e| MemoryError::Embedder(format!("bad bert config: {e}")))?;

        let tokenizer = Tokenizer::from_file(&files.tokenizer)
            .map_err(|e| MemoryError::Embedder(format!("load tokenizer: {e}")))?;

        let bytes = std::fs::read(&files.weights)
            .map_err(|e| MemoryError::Embedder(format!("read weights: {e}")))?;
        let vb = VarBuilder::from_buffered_safetensors(bytes, DType::F32, &device)
            .map_err(|e| MemoryError::Embedder(format!("load safetensors: {e}")))?;

        // Checkpoints exist both with (`bert.*`) and without the prefix;
        // let the model's own loader try, then fall back to the prefixed form.
        let model = match BertModel::load(vb.clone(), &bert_config) {
            Ok(m) => m,
            Err(e) => BertModel::load(vb.pp("bert"), &bert_config).map_err(|e2| {
                MemoryError::Embedder(format!("load bert weights: {e} / prefixed: {e2}"))
            })?,
        };

        let dims = if preset_dims > 0 {
            preset_dims
        } else {
            bert_config.hidden_size
        };
        let max_len = bert_config.max_position_embeddings.min(512);

        Ok(LocalEmbedder {
            model,
            tokenizer,
            device,
            pooling,
            name,
            dims,
            max_len,
            batch_size: 8,
        })
    }

    pub fn pooling(&self) -> Pooling {
        self.pooling
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut encodings = Vec::with_capacity(texts.len());
        for text in texts {
            let enc = self
                .tokenizer
                .encode(text.as_str(), true)
                .map_err(|e| MemoryError::Embedder(format!("tokenize: {e}")))?;
            let ids: Vec<u32> = enc.get_ids().iter().copied().take(self.max_len).collect();
            let mask: Vec<u32> = enc
                .get_attention_mask()
                .iter()
                .copied()
                .take(self.max_len)
                .collect();
            let types: Vec<u32> = enc
                .get_type_ids()
                .iter()
                .copied()
                .take(self.max_len)
                .collect();
            encodings.push((ids, mask, types));
        }

        let seq_len = encodings
            .iter()
            .map(|(ids, _, _)| ids.len())
            .max()
            .unwrap_or(1);
        let pad = |v: &Vec<u32>| {
            let mut padded = v.clone();
            padded.resize(seq_len, 0u32);
            padded
        };

        let input_ids: Vec<Vec<u32>> = encodings.iter().map(|(ids, _, _)| pad(ids)).collect();
        let mask_rows: Vec<Vec<u32>> = encodings.iter().map(|(_, m, _)| pad(m)).collect();
        let type_rows: Vec<Vec<u32>> = encodings.iter().map(|(_, _, t)| pad(t)).collect();

        let input_ids = Tensor::new(input_ids, &self.device).map_err(candle_err("input_ids"))?;
        let mask_f32: Vec<Vec<f32>> = mask_rows
            .iter()
            .map(|row| row.iter().map(|&m| m as f32).collect())
            .collect();
        let attention_mask =
            Tensor::new(mask_f32, &self.device).map_err(candle_err("attention_mask"))?;
        let token_type_ids =
            Tensor::new(type_rows, &self.device).map_err(candle_err("token_type_ids"))?;

        // (batch, seq_len, hidden) — candle's signature is
        // (input_ids, token_type_ids, attention_mask: Option)
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))
            .and_then(|t| t.to_dtype(DType::F32))
            .map_err(|e| MemoryError::Embedder(format!("forward: {e}")))?;
        let (b, s, h) = hidden
            .dims3()
            .map_err(|e| MemoryError::Embedder(format!("unexpected model output: {e}")))?;
        let data = hidden
            .flatten_all()
            .and_then(|t| t.to_vec1::<f32>())
            .map_err(|e| MemoryError::Embedder(format!("read output: {e}")))?;

        let mut out = Vec::with_capacity(b);
        for row in 0..b {
            let base = row * s * h;
            let mut vec = vec![0.0f32; h];
            match self.pooling {
                Pooling::Cls => {
                    vec.copy_from_slice(&data[base..base + h]);
                }
                Pooling::Mean => {
                    let mask_row = &mask_rows[row];
                    let mut count = 0.0f32;
                    for (t, m) in mask_row.iter().enumerate() {
                        if *m > 0 {
                            count += 1.0;
                            let tok_base = base + t * h;
                            for (d, v) in vec.iter_mut().enumerate() {
                                *v += data[tok_base + d];
                            }
                        }
                    }
                    if count > 0.0 {
                        for v in vec.iter_mut() {
                            *v /= count;
                        }
                    }
                }
            }
            normalize(&mut vec);
            out.push(vec);
        }
        Ok(out)
    }
}

fn candle_err(what: &'static str) -> impl Fn(candle_core::Error) -> MemoryError {
    move |e| MemoryError::Embedder(format!("{what}: {e}"))
}

fn fetch_from_hub(repo_id: &str, cache_dir: Option<&Path>) -> Result<ModelFiles> {
    let mut builder = hf_hub::api::sync::ApiBuilder::new();
    if let Some(dir) = cache_dir {
        builder = builder.with_cache_dir(dir.to_path_buf());
    }
    let api = builder
        .build()
        .map_err(|e| MemoryError::Embedder(format!("hf hub api: {e}")))?;
    let repo = api.model(repo_id.to_string());
    let get = |name: &str| -> Result<PathBuf> {
        repo.get(name)
            .map_err(|e| MemoryError::Embedder(format!("download {repo_id}/{name}: {e}")))
    };
    Ok(ModelFiles {
        config: get("config.json")?,
        tokenizer: get("tokenizer.json")?,
        weights: get("model.safetensors")?,
    })
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    let mut s = String::new();
    std::fs::File::open(path)
        .and_then(|mut f| f.read_to_string(&mut s))
        .map_err(|e| MemoryError::Embedder(format!("read {}: {e}", path.display())))?;
    serde_json::from_str(&s)
        .map_err(|e| MemoryError::Embedder(format!("parse {}: {e}", path.display())))
}

/// Candle's `BertConfig` is strict about field types; build it from the HF
/// config.json with explicit defaults so any BERT-family checkpoint parses.
fn bert_config_from_json(v: &serde_json::Value) -> std::result::Result<BertConfig, String> {
    let need = |key: &str| -> std::result::Result<u64, String> {
        v.get(key)
            .and_then(|x| x.as_u64())
            .ok_or_else(|| format!("config.json missing `{key}`"))
    };
    let curated = serde_json::json!({
        "vocab_size": need("vocab_size")?,
        "hidden_size": need("hidden_size")?,
        "num_hidden_layers": need("num_hidden_layers")?,
        "num_attention_heads": need("num_attention_heads")?,
        "intermediate_size": need("intermediate_size")?,
        "hidden_act": v.get("hidden_act").and_then(|x| x.as_str()).unwrap_or("gelu"),
        "max_position_embeddings": v.get("max_position_embeddings").and_then(|x| x.as_u64()).unwrap_or(512),
        "type_vocab_size": v.get("type_vocab_size").and_then(|x| x.as_u64()).unwrap_or(2),
        "initializer_range": v.get("initializer_range").and_then(|x| x.as_f64()).unwrap_or(0.02),
        "layer_norm_eps": v.get("layer_norm_eps").and_then(|x| x.as_f64()).unwrap_or(1e-12),
        "hidden_dropout_prob": v.get("hidden_dropout_prob").and_then(|x| x.as_f64()).unwrap_or(0.1),
        "attention_probs_dropout_prob": v.get("attention_probs_dropout_prob").and_then(|x| x.as_f64()).unwrap_or(0.1),
        "pad_token_id": v.get("pad_token_id").and_then(|x| x.as_u64()).unwrap_or(0),
        "position_embedding_type": v.get("position_embedding_type").and_then(|x| x.as_str()).unwrap_or("absolute"),
        "use_cache": false,
        "classifier_dropout": serde_json::Value::Null,
        "model_type": "bert",
    });
    serde_json::from_value(curated).map_err(|e| e.to_string())
}

impl Embedder for LocalEmbedder {
    fn name(&self) -> &'static str {
        self.name
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn default_min_similarity(&self) -> f32 {
        0.30
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.batch_size.max(1)) {
            out.extend(self.embed_batch(chunk)?);
        }
        Ok(out)
    }
}
