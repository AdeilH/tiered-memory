//! Real-model smoke test for the embedded local embedder (feature `local`).
//! Downloads/caches `sentence-transformers/all-MiniLM-L6-v2` on first run:
//!
//! ```bash
//! cargo test --features local --test local_embedder -- --ignored --nocapture
//! ```

#![cfg(feature = "local")]

use tiered_memory::vector::cosine;
use tiered_memory::{Embedder, LocalEmbedder, LocalEmbedderConfig};

#[test]
#[ignore = "downloads a ~90MB model on first run"]
fn minilm_embeds_and_orders_semantics() {
    let e = LocalEmbedder::load(LocalEmbedderConfig {
        preset: "minilm".into(),
        dir: None,
        cache_dir: Some(std::env::temp_dir().join("tiered-memory-models")),
    })
    .expect("load minilm");

    assert_eq!(e.dims(), 384);
    assert_eq!(e.name(), "minilm");

    let v = e
        .embed(&[
            "The learner prefers analogies from games and play".to_string(),
            "Analogies drawn from video games help this learner".to_string(),
            "Gardening tips for growing tomatoes in spring".to_string(),
        ])
        .expect("embed");

    assert_eq!(v.len(), 3);
    for vec in &v {
        assert_eq!(vec.len(), 384);
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "vectors must be normalized");
    }
    let close = cosine(&v[0], &v[1]);
    let far = cosine(&v[0], &v[2]);
    println!("close={close:.3} far={far:.3}");
    assert!(
        close > far,
        "semantic ordering must hold (close={close}, far={far})"
    );
    assert!(close > 0.5, "paraphrases should be very similar");
    assert!(far < close - 0.15, "unrelated text should be clearly below");
}
