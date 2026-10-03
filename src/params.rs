//! Learner-parameter merging — the "adjusts parameters according to the learner"
//! half of the engine.
//!
//! Parameters live on memory records as key→[`ParamValue`] assertions. Reads
//! never mutate anything: the adjusted parameter set is *derived* at query time
//! by ranking every visible assertion per key with
//!
//! `rank = layer_weight × confidence × recency × usage`
//!
//! and letting the nearest layer win (L1 > L2 > L3), exactly like a cache hit
//! serving from the closest level. Deeper-layer values that disagree with the
//! winner are surfaced as `alternatives` so callers can detect conflicts.

use crate::types::{Level, MemoryRecord, ParamValue, DAY_MS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamAlternative {
    pub value: ParamValue,
    pub source: Level,
    pub confidence: f32,
}

/// The adjusted value for one parameter key, with its provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSuggestion {
    pub key: String,
    pub value: ParamValue,
    /// Layer the winning value came from.
    pub source: Level,
    pub confidence: f32,
    pub updated_at_ms: u64,
    /// Conflicting values seen in other layers, best-first.
    pub alternatives: Vec<ParamAlternative>,
}

#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub level: Level,
    pub value: &'a ParamValue,
    pub confidence: f32,
    pub updated_at_ms: u64,
    pub use_count: u32,
}

fn recency(age_ms: u64, now_ms: u64, half_life_days: f32) -> f32 {
    let age_days = now_ms.saturating_sub(age_ms) as f32 / DAY_MS as f32;
    2f32.powf(-age_days / half_life_days.max(0.01))
}

pub(crate) fn rank(c: &Candidate<'_>, now_ms: u64, half_life_days: f32) -> f32 {
    let usage = 1.0 + 0.1 * (1.0 + c.use_count as f32).ln();
    c.level.weight()
        * c.confidence.max(0.01)
        * recency(c.updated_at_ms, now_ms, half_life_days)
        * usage
}

/// Derive the adjusted parameter set from a list of *already visibility-filtered*
/// records (the caller decides which records a project may see).
pub fn collect_suggestions(
    records: &[&MemoryRecord],
    now_ms: u64,
    half_life_days: f32,
    numeric_tolerance: f64,
) -> Vec<ParamSuggestion> {
    // key -> candidates (dedup identical (key, level, value) repeats from
    // promoted copies: prefer the shallowest level for the same value)
    let mut by_key: BTreeMap<String, Vec<Candidate<'_>>> = BTreeMap::new();
    for r in records {
        for (key, value) in &r.params {
            let cand = Candidate {
                level: r.level,
                value,
                confidence: r.confidence,
                updated_at_ms: r.last_used_at_ms.max(r.created_at_ms),
                use_count: r.use_count,
            };
            let list = by_key.entry(key.clone()).or_default();
            // A promoted L1 copy duplicates its L2/L3 origin; count each layer once per value.
            if list
                .iter()
                .any(|c| c.level == cand.level && c.value.compatible(value, numeric_tolerance))
            {
                // keep the fresher one of the duplicates
                if let Some(existing) = list
                    .iter_mut()
                    .find(|c| c.level == cand.level && c.value.compatible(value, numeric_tolerance))
                {
                    if cand.updated_at_ms > existing.updated_at_ms {
                        *existing = cand;
                    }
                }
                continue;
            }
            list.push(cand);
        }
    }

    let mut out = Vec::with_capacity(by_key.len());
    for (key, mut cands) in by_key {
        cands.sort_by(|a, b| {
            rank(b, now_ms, half_life_days)
                .partial_cmp(&rank(a, now_ms, half_life_days))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let winner = &cands[0];
        let mut alternatives = Vec::new();
        let mut seen_values: BTreeSet<String> = BTreeSet::new();
        seen_values.insert(winner.value.as_text().to_lowercase());
        for c in &cands {
            let vt = c.value.as_text().to_lowercase();
            if seen_values.insert(vt) {
                alternatives.push(ParamAlternative {
                    value: c.value.clone(),
                    source: c.level,
                    confidence: c.confidence,
                });
            }
            if alternatives.len() >= 3 {
                break;
            }
        }
        out.push(ParamSuggestion {
            key,
            value: winner.value.clone(),
            source: winner.level,
            confidence: winner.confidence,
            updated_at_ms: winner.updated_at_ms,
            alternatives,
        });
    }
    out
}

/// Consensus value across projects, used when lifting a parameter to L3.
/// Returns `None` when the projects disagree (the engine then leaves the
/// parameter project-local instead of guessing).
pub fn consensus_value(values: &[&ParamValue], numeric_tolerance: f64) -> Option<ParamValue> {
    if values.is_empty() {
        return None;
    }
    match values[0] {
        ParamValue::Number(_) => {
            let mut nums = Vec::with_capacity(values.len());
            for v in values {
                match v {
                    ParamValue::Number(n) => nums.push(*n),
                    _ => return None,
                }
            }
            let max = nums.iter().cloned().fold(f64::MIN, f64::max);
            let min = nums.iter().cloned().fold(f64::MAX, f64::min);
            if max - min > numeric_tolerance {
                return None;
            }
            Some(ParamValue::Number(
                nums.iter().sum::<f64>() / nums.len() as f64,
            ))
        }
        ParamValue::Text(t) => {
            let low = t.to_lowercase();
            if values
                .iter()
                .all(|v| matches!(v, ParamValue::Text(s) if s.to_lowercase() == low))
            {
                Some(values[0].clone())
            } else {
                None
            }
        }
        ParamValue::Bool(b) => {
            if values
                .iter()
                .all(|v| matches!(v, ParamValue::Bool(x) if *x == *b))
            {
                Some(values[0].clone())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(level: Level, key: &str, value: ParamValue, conf: f32, at_ms: u64) -> MemoryRecord {
        let mut params = BTreeMap::new();
        params.insert(key.to_string(), value);
        MemoryRecord {
            id: format!("r-{at_ms}-{key}"),
            user_id: "u".into(),
            text: "t".into(),
            vector: vec![],
            level,
            project_id: None,
            group: None,
            topic: None,
            kind: crate::types::MemoryKind::Feedback,
            params,
            key_hint: Some(key.into()),
            confidence: conf,
            pinned: false,
            created_at_ms: at_ms,
            last_used_at_ms: at_ms,
            use_count: 0,
            origin: None,
            expires_at_ms: None,
        }
    }

    #[test]
    fn nearer_layer_wins_and_conflicts_become_alternatives() {
        let l1 = rec(Level::L1, "difficulty", ParamValue::Number(0.3), 0.9, 1_000);
        let l3 = rec(Level::L3, "difficulty", ParamValue::Number(0.7), 0.9, 2_000);
        let style = rec(
            Level::L3,
            "analogy_domain",
            ParamValue::Text("games".into()),
            0.8,
            3_000,
        );
        let out = collect_suggestions(&[&l1, &l3, &style], 60_000, 45.0, 0.15);
        let diff = out.iter().find(|s| s.key == "difficulty").unwrap();
        assert_eq!(diff.value, ParamValue::Number(0.3));
        assert_eq!(diff.source, Level::L1);
        assert!(diff
            .alternatives
            .iter()
            .any(|a| a.value == ParamValue::Number(0.7) && a.source == Level::L3));
    }

    #[test]
    fn fresher_same_layer_wins() {
        let old = rec(
            Level::L1,
            "style",
            ParamValue::Text("long".into()),
            0.9,
            1_000,
        );
        let new = rec(
            Level::L1,
            "style",
            ParamValue::Text("short".into()),
            0.9,
            9_000,
        );
        let out = collect_suggestions(&[&old, &new], 10_000, 45.0, 0.15);
        assert_eq!(out[0].value, ParamValue::Text("short".into()));
    }

    #[test]
    fn consensus_requires_agreement() {
        let n04 = ParamValue::Number(0.4);
        let n05 = ParamValue::Number(0.5);
        let vals = vec![&n04, &n05];
        assert_eq!(consensus_value(&vals, 0.15), Some(ParamValue::Number(0.45)));
        let n02 = ParamValue::Number(0.2);
        let n09 = ParamValue::Number(0.9);
        let conflicting = vec![&n02, &n09];
        assert_eq!(consensus_value(&conflicting, 0.15), None);
        let games_low = ParamValue::Text("games".into());
        let games_upper = ParamValue::Text("Games".into());
        let texts = vec![&games_low, &games_upper];
        assert_eq!(
            consensus_value(&texts, 0.15),
            Some(ParamValue::Text("games".into()))
        );
    }
}
