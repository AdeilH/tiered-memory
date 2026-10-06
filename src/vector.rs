//! Small vector helpers: similarity, normalization, id generation.

/// Dot product of two equal-length slices.
///
/// Every [`crate::embed::Embedder`] returns L2-normalized vectors (trait
/// contract), so for stored/query vectors this *is* the cosine similarity —
/// without recomputing either norm. Hot scans (recall, dedupe, consolidation)
/// therefore use `dot` directly; `cosine` remains for untrusted inputs.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Cosine similarity with a guard for zero vectors. For the engine's own
/// L2-normalized embeddings this equals [`dot`]; prefer `dot` in hot loops.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let na = (a.iter().map(|x| x * x).sum::<f32>()).sqrt();
    let nb = (b.iter().map(|x| x * x).sum::<f32>()).sqrt();
    if na < 1e-12 || nb < 1e-12 {
        return 0.0;
    }
    dot(a, b) / (na * nb)
}

/// Normalize a vector in place to unit length.
pub fn normalize(v: &mut [f32]) {
    let n = (v.iter().map(|x| x * x).sum::<f32>()).sqrt();
    if n > 1e-12 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// FNV-1a 64-bit — deterministic string hash used by the hashing embedder and ids.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Generate a reasonably unique, sortable-ish id: `{prefix}{time:x}-{counter:x}-{hash:x}`.
pub fn gen_id(prefix: &str, text: &str, now_ms: u64, counter: u64) -> String {
    let h = fnv1a64(text.as_bytes()) & 0xffff;
    format!("{prefix}{now_ms:x}-{counter:x}-{h:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn normalize_to_unit() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b"hello"), fnv1a64(b"hello"));
        assert_ne!(fnv1a64(b"hello"), fnv1a64(b"hellp"));
    }
}
