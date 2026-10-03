//! Dependency-free deterministic embedder: word unigrams + character
//! 3-grams hashed into a fixed-width bag-of-features, sublinear TF, L2-normalized.
//!
//! Not semantic — it measures lexical overlap — but it is instant, offline,
//! deterministic and platform-stable, which makes it the default for tests,
//! CI and air-gapped use. Swap in `local` or `http` for real semantics.

use crate::embed::Embedder;
use crate::error::Result;
use crate::vector::{fnv1a64, normalize};

pub struct HashingEmbedder {
    dims: usize,
}

impl HashingEmbedder {
    pub fn new(dims: usize) -> Self {
        HashingEmbedder {
            dims: dims.clamp(64, 4096),
        }
    }
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        tokens.push(format!("w:{word}"));
        if word.chars().count() >= 4 {
            let chars: Vec<char> = word.chars().collect();
            for w in chars.windows(3) {
                tokens.push(format!("g:{}", w.iter().collect::<String>()));
            }
        }
    }
    tokens
}

impl Embedder for HashingEmbedder {
    fn name(&self) -> &'static str {
        "hashing"
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn default_min_similarity(&self) -> f32 {
        0.15
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let mut vec = vec![0.0f32; self.dims];
            let mut tf: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
            for tok in tokenize(text) {
                *tf.entry(fnv1a64(tok.as_bytes())).or_insert(0) += 1;
            }
            for (h, count) in tf {
                let idx = (h % self.dims as u64) as usize;
                vec[idx] += 1.0 + (count as f32).ln();
            }
            normalize(&mut vec);
            out.push(vec);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_and_deterministic() {
        let e = HashingEmbedder::new(512);
        let t = "The learner prefers analogies from games.".to_string();
        let a = e.embed(std::slice::from_ref(&t)).unwrap().remove(0);
        let b = e.embed(&[t]).unwrap().remove(0);
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(a.len(), 512);
    }

    #[test]
    fn lexical_similarity_orders_sensibly() {
        let e = HashingEmbedder::new(512);
        let v = e
            .embed(&[
                "prefer zustand for state management".to_string(),
                "prefer zustand for state management in frontend".to_string(),
                "gardening tips for tomatoes and basil".to_string(),
            ])
            .unwrap();
        let close = crate::vector::cosine(&v[0], &v[1]);
        let far = crate::vector::cosine(&v[0], &v[2]);
        assert!(close > far, "close={close} far={far}");
        assert!(close > 0.3);
        // unrelated texts share only hash-collision noise (~0.15 at 512 dims)
        assert!(far < 0.2);
    }
}
