//! Core data model: memory levels (the L1/L2/L3 cache analogy), memory records,
//! structured parameter values, and per-user databases.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Cache-style memory level.
///
/// * `L1` — hot, tiny, scoped to **one project** the learner is in right now.
/// * `L2` — warm, scoped to a project and surfaced to **related scopes**
///   (other components of the same project, similar projects).
/// * `L3` — cold, large, **global** — traits that hold across every project.
///
/// Derived ordering is `L1 < L2 < L3` (depth); recall always probes L1 first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    L1,
    L2,
    L3,
}

impl Level {
    pub fn rank(self) -> u8 {
        match self {
            Level::L1 => 0,
            Level::L2 => 1,
            Level::L3 => 2,
        }
    }

    /// Multiplier applied to similarity when ranking recall hits — records
    /// serving from a nearer layer win ties against deeper, equally similar ones.
    pub fn weight(self) -> f32 {
        match self {
            Level::L1 => 1.0,
            Level::L2 => 0.9,
            Level::L3 => 0.85,
        }
    }

    pub fn capacity_in(self, cfg: &crate::engine::EngineConfig) -> usize {
        match self {
            Level::L1 => cfg.l1_capacity,
            Level::L2 => cfg.l2_capacity,
            Level::L3 => cfg.l3_capacity,
        }
    }
}

/// What kind of knowledge a memory carries. Affects routing (traits are global)
/// and consolidation (traits are the only records the engine *writes* on its own).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// An asserted preference ("prefers analogies from games").
    Preference,
    /// A durable attribute of the learner ("works in Rust daily").
    Trait,
    /// A parameter update captured from feedback events.
    Feedback,
    /// A distilled fact/summary worth remembering.
    Summary,
    /// Anything else.
    Note,
}

/// A structured, typed value a learner parameter can take.
///
/// Untagged so JSON is ergonomic: `0.4`, `"games"`, `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamValue {
    Number(f64),
    Text(String),
    Bool(bool),
}

impl ParamValue {
    /// Do two values agree closely enough to be "the same preference"?
    /// Numbers agree inside `tolerance`; text/bool must match exactly (case-insensitive).
    pub fn compatible(&self, other: &ParamValue, tolerance: f64) -> bool {
        match (self, other) {
            (ParamValue::Number(a), ParamValue::Number(b)) => (a - b).abs() <= tolerance,
            (ParamValue::Text(a), ParamValue::Text(b)) => a.eq_ignore_ascii_case(b),
            (ParamValue::Bool(a), ParamValue::Bool(b)) => a == b,
            _ => false,
        }
    }

    pub fn as_text(&self) -> String {
        match self {
            ParamValue::Number(n) => {
                // Pretty-print integers without a trailing ".0".
                if n.fract() == 0.0 && n.abs() < 1e15 {
                    format!("{}", *n as i64)
                } else {
                    format!("{n}")
                }
            }
            ParamValue::Text(t) => t.clone(),
            ParamValue::Bool(b) => b.to_string(),
        }
    }
}

/// One persisted memory — a cache line of learner knowledge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub user_id: String,
    /// Natural-language content; this is what gets embedded and matched.
    pub text: String,
    /// Normalized embedding of `text` under the store's embedder fingerprint.
    pub vector: Vec<f32>,
    /// Which layer this record lives in (L1 project / L2 related / L3 global).
    pub level: Level,
    /// Owning project for L1/L2 records; `None` for L3 — and for L2 records
    /// owned by a *group* rather than a single project (see `group`).
    pub project_id: Option<String>,
    /// For L2 group-owned records: the group this fact is about ("all my CLI
    /// projects use clap"). Visible to every member project of that group.
    /// Never set on L1/L3 records; the reserved label `none` is not a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Optional category slug ("writing-style", "flow", "preferences") used to
    /// file the human-readable L2 mirrors into per-topic files. Pure
    /// organization — recall and parameters ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub kind: MemoryKind,
    /// Structured parameter assertions carried by this memory.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
    /// For parameter-carrying records (feedback, lifted traits): the canonical
    /// parameter key. Enables upsert-by-key instead of upsert-by-similarity.
    #[serde(default)]
    pub key_hint: Option<String>,
    /// 0..1 — how firmly this memory is held.
    pub confidence: f32,
    /// Pinned memories are never evicted, expired, or forgotten.
    #[serde(default)]
    pub pinned: bool,
    pub created_at_ms: u64,
    pub last_used_at_ms: u64,
    #[serde(default)]
    pub use_count: u32,
    /// For promoted copies: the id of the canonical record deeper in the hierarchy.
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
}

impl MemoryRecord {
    pub fn canonical_id(&self) -> &str {
        self.origin.as_deref().unwrap_or(&self.id)
    }

    /// The layer this record *serves at* for a viewer in `project`. A used
    /// project's L1 lines are visible cross-project, but they are not the
    /// viewer's own hot line — they surface from the warm (L2) tier, keeping
    /// "L1 in the output = this project's hot line" true everywhere.
    pub fn serving_level_for(&self, project: Option<&str>) -> Level {
        if self.level == Level::L1
            && self.project_id.is_some()
            && self.project_id.as_deref() != project
        {
            Level::L2
        } else {
            self.level
        }
    }

    pub fn is_expired(&self, now_ms: u64) -> bool {
        !self.pinned && self.expires_at_ms.map(|t| t <= now_ms).unwrap_or(false)
    }
}

/// A project as the engine sees it: a scope key plus a descriptor whose
/// embedding decides which *other* projects it shares L2 with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub project_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Components of this project ("frontend", "backend", ...) — all share L2.
    #[serde(default)]
    pub components: Vec<String>,
    /// Text whose embedding is used for similar-project matching.
    pub descriptor: String,
    #[serde(default)]
    pub descriptor_vector: Vec<f32>,
    /// Project ids whose L2 memories surface when recalling for this project.
    #[serde(default)]
    pub similar: Vec<String>,
    /// Project ids this project explicitly **uses**: their L1 *and* L2
    /// memories surface here (serving from the warm L2 tier). Directional —
    /// `b.uses = [a]` lets b see a's memories, never the reverse. User-managed
    /// via `tiered-memory use <project>` / the hand-editable `uses.txt`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<String>,
    /// The project's L2 group (a family of projects sharing warm memories,
    /// e.g. `rust-clis`). The reserved value `none` means the user explicitly
    /// confirmed this project belongs to no group — don't ask again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub created_at_ms: u64,
}

/// Everything the engine persists for one learner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserDb {
    pub version: u32,
    /// Fingerprint (`name:dims`) of the embedder that produced every vector here.
    pub embedder: String,
    pub dims: usize,
    #[serde(default)]
    pub records: Vec<MemoryRecord>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectInfo>,
}

impl UserDb {
    pub fn new(embedder_fp: String, dims: usize) -> Self {
        UserDb {
            version: 1,
            embedder: embedder_fp,
            dims,
            records: Vec::new(),
            projects: BTreeMap::new(),
        }
    }

    pub fn count_in(&self, level: Level) -> usize {
        self.records.iter().filter(|r| r.level == level).count()
    }
}

pub(crate) const DAY_MS: u64 = 86_400_000;
