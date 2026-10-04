//! The layered memory engine.
//!
//! Implements CPU-cache semantics for learner memory:
//!
//! * **Read path** (`recall`) probes L1 → L2 → L3, merges and ranks hits
//!   (similarity × layer weight × confidence × recency), deduplicates
//!   promoted copies against their canonical records, and *touches* what it
//!   served. Repeatedly-used deeper-layer memories are copied up into L1
//!   (write-allocate) so hot knowledge migrates toward the learner. Projects
//!   can explicitly `use` another project: its L1+L2 lines surface in the
//!   borrower's warm tier (directional, hot ones promote like any L2 hit).
//! * **Write path** (`remember`/`feedback`) lands new knowledge in L1 of the
//!   current project (or L3 for global traits), deduplicates by key or by
//!   near-identical content, and enforces layer capacity. Evicted L1
//!   memories are written back into L2 (write-back), never silently lost.
//! * **Maintenance** (`consolidate`) expires TTLs, forgets low-confidence
//!   memories, merges near-duplicates, and *lifts* parameters that keep
//!   agreeing across ≥ N projects into a global L3 trait — "the learner
//!   consistently prefers X" — which then serves every new project.
//! * **Parameters** (`adjusted_parameters`) derives the personalized
//!   parameter set at read time; the nearest layer wins per key, conflicts
//!   surface as alternatives instead of being hidden.

use crate::embed::Embedder;
use crate::error::{MemoryError, Result};
use crate::params::{self, ParamSuggestion};
use crate::store::MemoryStore;
use crate::types::{Level, MemoryKind, MemoryRecord, ParamValue, ProjectInfo, UserDb, DAY_MS};
use crate::vector::{cosine, gen_id};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reserved group label: `group set none` records that the user explicitly
/// confirmed the project belongs to **no** L2 group (the skill stops asking).
/// Not a real group — every engine path normalizes it away.
pub const NO_GROUP: &str = "none";

/// `Some("none")` → `None`; real groups pass through.
pub fn effective_group(group: Option<&str>) -> Option<&str> {
    group.filter(|g| *g != NO_GROUP)
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|p| MemoryError::Storage(format!("lock poisoned: {p}")))
}

/// An all-zero embedding carries no signal — cosine against it is NaN, so it
/// silently never matches anything and just poisons the store. That only
/// happens when the embedder is broken (failed model load, empty feature
/// extraction), so refuse it loudly instead of persisting it.
fn ensure_embedded(vector: &[f32], what: &str) -> Result<()> {
    if vector.iter().all(|&v| v == 0.0) {
        return Err(MemoryError::Embedder(format!(
            "embedder returned an all-zero vector for {what} — check the embedding backend (TM_EMBEDDER) and the configured model"
        )));
    }
    Ok(())
}

/// Tunable policy knobs. Defaults are sane for a single-learner local service.
#[derive(Clone)]
pub struct EngineConfig {
    /// L1 is small and hot — the project the learner is in right now.
    pub l1_capacity: usize,
    pub l2_capacity: usize,
    pub l3_capacity: usize,
    pub default_k: usize,
    /// `None` = fall back to the embedder's own suggestion.
    pub default_min_similarity: Option<f32>,
    /// Parameter recency half-life when ranking competing values.
    pub half_life_days: f32,
    /// Same-scope content this similar is treated as an update, not a new memory.
    pub dup_threshold: f32,
    /// Consolidation merges same-scope memories this similar into one.
    pub merge_threshold: f32,
    /// Projects whose descriptors embed this close share L2.
    pub similar_project_threshold: f32,
    /// Copy repeatedly-recalled L2/L3 memories into L1 on recall.
    pub write_allocate: bool,
    /// Hits needed before a deeper memory is promoted into L1.
    pub promote_min_hits: u32,
    /// Run consolidation automatically every N writes (0 disables).
    pub auto_consolidate_every: u32,
    /// Memories below this confidence are forgotten during consolidation.
    pub confidence_floor: f32,
    /// Numeric parameters agree when within this tolerance.
    pub numeric_param_tolerance: f64,
    /// Lift a parameter to L3 once it agrees across this many projects.
    pub trait_lift_min_projects: usize,
    /// Injectable clock for deterministic tests.
    pub now: NowFn,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            l1_capacity: 128,
            l2_capacity: 1024,
            l3_capacity: 4096,
            default_k: 6,
            default_min_similarity: None,
            half_life_days: 45.0,
            dup_threshold: 0.96,
            merge_threshold: 0.92,
            similar_project_threshold: 0.5,
            write_allocate: true,
            promote_min_hits: 3,
            auto_consolidate_every: 50,
            confidence_floor: 0.05,
            numeric_param_tolerance: 0.15,
            trait_lift_min_projects: 2,
            now: Arc::new(system_now_ms),
        }
    }
}

impl EngineConfig {
    /// Overrides from env: `TM_L1_CAPACITY`, `TM_L2_CAPACITY`, `TM_L3_CAPACITY`,
    /// `TM_CONSOLIDATE_EVERY`, `TM_WRITE_ALLOCATE` (0/1), `TM_MIN_SIMILARITY`.
    pub fn from_env() -> Self {
        let mut c = Self::default();
        let env_usize = |var: &str, cur: usize| -> usize {
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(cur)
        };
        c.l1_capacity = env_usize("TM_L1_CAPACITY", c.l1_capacity);
        c.l2_capacity = env_usize("TM_L2_CAPACITY", c.l2_capacity);
        c.l3_capacity = env_usize("TM_L3_CAPACITY", c.l3_capacity);
        if let Ok(v) = std::env::var("TM_CONSOLIDATE_EVERY") {
            if let Ok(n) = v.parse::<u32>() {
                c.auto_consolidate_every = n;
            }
        }
        if let Ok(v) = std::env::var("TM_WRITE_ALLOCATE") {
            c.write_allocate = v != "0";
        }
        if let Ok(v) = std::env::var("TM_MIN_SIMILARITY") {
            if let Ok(f) = v.parse::<f32>() {
                c.default_min_similarity = Some(f);
            }
        }
        c
    }
}

// ---------------------------------------------------------------------------
// Public request/response types (also the HTTP API contract)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ProjectInput {
    pub user: String,
    pub project_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Components of this project ("frontend", "backend", ...).
    #[serde(default)]
    pub components: Vec<String>,
    /// Free text describing the project; drives similar-project matching.
    /// Defaults to `name + tags + components`.
    #[serde(default)]
    pub descriptor: Option<String>,
    /// L2 group to assign at registration. Omitted → an existing project keeps
    /// its current group (re-init never wipes it).
    #[serde(default)]
    pub group: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RememberInput {
    pub user: String,
    pub text: String,
    #[serde(default)]
    pub project_id: Option<String>,
    /// Defaults: `trait` → global trait; with `key_hint` → `feedback`;
    /// otherwise `preference` in the project (L1) or globally (L3).
    #[serde(default)]
    pub kind: Option<MemoryKind>,
    /// Structured parameter assertions carried by this memory.
    #[serde(default)]
    pub params: Option<BTreeMap<String, ParamValue>>,
    /// Canonical parameter key for upsert-by-key semantics.
    #[serde(default)]
    pub key_hint: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub ttl_days: Option<f64>,
    /// Explicit layer placement; omit for automatic routing.
    #[serde(default)]
    pub level: Option<Level>,
    /// Write an L2 record owned by a **group** of projects ("all my CLIs use
    /// clap") instead of one project. Implies L2; the record is visible to
    /// every member of the group.
    #[serde(default)]
    pub group: Option<String>,
    /// Category slug ("writing-style", "flow", "preferences") used to file
    /// the human-readable L2 mirrors into per-topic files. Optional; free
    /// text is normalized (lowercase kebab-case).
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RememberOutcome {
    pub id: String,
    pub deduped: bool,
    pub demoted_to_l2: usize,
    pub auto_consolidated: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RecallInput {
    pub user: String,
    pub query: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub k: Option<usize>,
    #[serde(default)]
    pub min_similarity: Option<f32>,
    #[serde(default)]
    pub write_allocate: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub id: String,
    pub text: String,
    pub similarity: f32,
    pub score: f32,
    pub level: Level,
    pub kind: MemoryKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_hint: Option<String>,
    pub confidence: f32,
    pub use_count: u32,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallOutput {
    pub hits: Vec<RecallHit>,
    /// Layers the query probed, shallowest first.
    pub searched: Vec<Level>,
    /// Ids of L1 copies created by write-allocate during this recall.
    pub promoted: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedbackInput {
    pub user: String,
    pub key: String,
    pub value: ParamValue,
    #[serde(default)]
    pub project_id: Option<String>,
    /// Confidence weight (0..1), default 0.8.
    #[serde(default)]
    pub weight: Option<f32>,
    /// Record globally (L3) instead of in the project (L1).
    #[serde(default)]
    pub global: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForgetInput {
    pub user: String,
    /// Remove one memory (and its promoted copies) by id.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub level: Option<Level>,
    /// Delete everything for the user. Requires being set explicitly.
    #[serde(default)]
    pub all: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConsolidationReport {
    pub expired: usize,
    pub forgotten: usize,
    pub merged: usize,
    pub traits_lifted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Counts {
    pub l1: usize,
    pub l2: usize,
    pub l3: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct EngineStats {
    pub user: String,
    pub embedder: String,
    pub dims: usize,
    pub counts: Counts,
    pub capacities: Counts,
    pub projects: usize,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthInfo {
    pub ok: bool,
    pub version: String,
    pub embedder: String,
    pub dims: usize,
    pub users: usize,
}

/// One memory line as handed to the sync gather step (no vectors).
#[derive(Debug, Clone, Serialize)]
pub struct MemoryLine {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_hint: Option<String>,
    pub confidence: f32,
    pub pinned: bool,
}

/// The gathered state of all three layers for one project scope.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryContext {
    pub l1: Vec<MemoryLine>,
    pub l2: Vec<MemoryLine>,
    pub l3: Vec<MemoryLine>,
    pub params: Vec<ParamSuggestion>,
    /// The project's L2 group (family of related projects), if assigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Project ids sharing that group — L2 writes here surface to all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_members: Vec<String>,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct MemoryEngine {
    store: Arc<dyn MemoryStore>,
    embedder: Arc<dyn Embedder>,
    config: EngineConfig,
    users: RwLock<HashMap<String, Arc<Mutex<UserDb>>>>,
    id_counter: AtomicU64,
    writes: AtomicU32,
}

/// Visibility of one record for a recall in `project`. `similar` is the
/// project's explicit similarity links; `my_group` its L2 group; `groups`
/// maps every project id to its group (effective — `none` already removed);
/// `uses` the projects this one explicitly draws memory from.
fn visible_at(
    r: &MemoryRecord,
    project: Option<&str>,
    similar: &HashSet<String>,
    my_group: Option<&str>,
    groups: &HashMap<String, String>,
    uses: &HashSet<String>,
) -> bool {
    match r.level {
        // L1 serves its own project — the hot line — plus, when the viewer
        // explicitly uses that project, its hot lines too (serving from the
        // viewer's warm tier; see `MemoryRecord::serving_level_for`).
        Level::L1 => match (project, r.project_id.as_deref()) {
            (Some(p), Some(rp)) => rp == p || uses.contains(rp),
            _ => false,
        },
        // L2 serves its own project *and* related scopes: same project's other
        // components (same id), similar projects (explicit links), — when the
        // project has an L2 group — every other member of that group, and the
        // projects this one explicitly uses.
        Level::L2 => {
            // group-owned record ("all my CLIs use clap"): every member sees it
            if let Some(g) = effective_group(r.group.as_deref()) {
                return my_group == Some(g);
            }
            match (project, r.project_id.as_deref()) {
                (Some(p), Some(rp)) => {
                    rp == p
                        || similar.contains(rp)
                        || uses.contains(rp)
                        || my_group.is_some() && groups.get(rp).map(String::as_str) == my_group
                }
                _ => false,
            }
        }
        // L3 is global.
        Level::L3 => true,
    }
}

fn eviction_value(r: &MemoryRecord, now_ms: u64) -> f32 {
    let age_days =
        now_ms.saturating_sub(r.last_used_at_ms.max(r.created_at_ms)) as f32 / DAY_MS as f32;
    let recency = 2f32.powf(-age_days / 30.0);
    let key_boost = if r.key_hint.is_some() { 1.25 } else { 1.0 };
    r.confidence * recency * (1.0 + (1.0 + r.use_count as f32).ln()) * key_boost
}

/// The recall ranking formula (mirrored in docs/ARCHITECTURE.md):
/// `score = cosine × level_weight × (0.5 + 0.5·confidence) × (1 + 0.15·2^(−age/14d))`
/// — similarity dominates; layer, confidence and freshness nudge ties.
fn recall_score(sim: f32, level: Level, confidence: f32, age_ms: u64) -> f32 {
    let age_days = age_ms as f32 / DAY_MS as f32;
    sim * level.weight() * (0.5 + 0.5 * confidence) * (1.0 + 0.15 * 2f32.powf(-age_days / 14.0))
}

impl MemoryEngine {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Arc<dyn Embedder>,
        config: EngineConfig,
    ) -> Self {
        MemoryEngine {
            store,
            embedder,
            config,
            users: RwLock::new(HashMap::new()),
            id_counter: AtomicU64::new(1),
            writes: AtomicU32::new(0),
        }
    }

    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    // -- internals -----------------------------------------------------------

    fn next_id(&self, text: &str, now_ms: u64) -> String {
        let c = self.id_counter.fetch_add(1, Ordering::Relaxed);
        gen_id("m", text, now_ms, c)
    }

    fn user_db(&self, user: &str) -> Result<Arc<Mutex<UserDb>>> {
        if let Some(db) = self.users.read().expect("users lock").get(user) {
            return Ok(db.clone());
        }
        let mut w = self.users.write().expect("users lock");
        if let Some(db) = w.get(user) {
            return Ok(db.clone());
        }
        let db = match self.store.load(user)? {
            Some(db) => db,
            None => UserDb::new(self.embedder.fingerprint(), self.embedder.dims()),
        };
        let arc = Arc::new(Mutex::new(db));
        w.insert(user.to_string(), arc.clone());
        Ok(arc)
    }

    fn ensure_fingerprint(&self, db: &UserDb) -> Result<()> {
        let fp = self.embedder.fingerprint();
        if db.embedder != fp {
            Err(MemoryError::EmbedderMismatch {
                db: db.embedder.clone(),
                current: fp,
            })
        } else {
            Ok(())
        }
    }

    /// Keep a layer within capacity. L1 evictees that are canonical memories
    /// (not promoted copies) are demoted into L2 — write-back, nothing is lost.
    fn enforce_capacity(&self, db: &mut UserDb, level: Level, now_ms: u64) -> usize {
        let cap = level.capacity_in(&self.config);
        let mut demoted = 0;
        loop {
            let count = db.records.iter().filter(|r| r.level == level).count();
            if count <= cap {
                break;
            }
            let mut best: Option<(usize, f32)> = None;
            for (i, r) in db.records.iter().enumerate() {
                if r.level != level || r.pinned {
                    continue;
                }
                let v = eviction_value(r, now_ms);
                if best.map(|(_, bv)| v < bv).unwrap_or(true) {
                    best = Some((i, v));
                }
            }
            let Some((i, _)) = best else { break }; // all pinned → allow overflow
            let rec = db.records.remove(i);
            if level == Level::L1 && rec.origin.is_none() {
                demoted += 1;
                db.records.push(MemoryRecord {
                    level: Level::L2,
                    ..rec
                });
            }
            // promoted copies & L2/L3 overflow simply drop
        }
        demoted
    }

    /// Copy one deep (L2/L3) record up into L1 of `project` — write-allocate.
    /// Skips (returns `None`) when an L1 copy already exists; the original
    /// stays untouched and the copy carries `origin` so dedupe works.
    fn promote_into_l1(
        &self,
        db: &mut UserDb,
        user: &str,
        project: &str,
        idx: usize,
        now_ms: u64,
    ) -> Option<String> {
        let (id, text, vector, kind, params, key_hint, confidence, topic) = {
            let src = &db.records[idx];
            (
                src.id.clone(),
                src.text.clone(),
                src.vector.clone(),
                src.kind,
                src.params.clone(),
                src.key_hint.clone(),
                src.confidence,
                src.topic.clone(),
            )
        };
        let already = db.records.iter().any(|r| {
            r.level == Level::L1
                && r.project_id.as_deref() == Some(project)
                && (r.id == id || r.origin.as_deref() == Some(id.as_str()))
        });
        if already {
            return None;
        }
        let new_id = self.next_id(&text, now_ms);
        db.records.push(MemoryRecord {
            id: new_id.clone(),
            user_id: user.to_string(),
            text,
            vector,
            level: Level::L1,
            project_id: Some(project.to_string()),
            group: None,
            topic,
            kind,
            params,
            key_hint,
            confidence,
            pinned: false,
            created_at_ms: now_ms,
            last_used_at_ms: now_ms,
            use_count: 1,
            origin: Some(id),
            expires_at_ms: None,
        });
        Some(new_id)
    }

    /// Write-path steps 1–2: fold the incoming payload into an existing
    /// record at this scope — by parameter key first (re-asserting a key is
    /// an update, not a new line of cache), then by near-identical content.
    /// Returns the record id when an upsert happened.
    fn upsert_in_scope(
        db: &mut UserDb,
        level: Level,
        project_id: Option<&str>,
        inc: &Incoming,
        dup_threshold: f32,
    ) -> Option<String> {
        // 1) upsert by key
        if let Some(kh) = &inc.key_hint {
            if let Some(rec) = db.records.iter_mut().find(|r| {
                r.level == level
                    && r.project_id.as_deref() == project_id
                    && r.key_hint.as_deref() == Some(kh.as_str())
            }) {
                rec.text = inc.text.clone();
                rec.vector = inc.vector.clone();
                for (k, v) in inc.params.clone() {
                    rec.params.insert(k, v);
                }
                rec.confidence = (rec.confidence + inc.confidence) / 2.0;
                rec.last_used_at_ms = inc.now;
                rec.expires_at_ms = inc.expires_at_ms;
                rec.use_count += 1;
                if inc.topic.is_some() {
                    rec.topic = inc.topic.clone();
                }
                if let Some(p) = inc.pinned {
                    rec.pinned = p;
                }
                return Some(rec.id.clone());
            }
        }

        // 2) near-duplicate content at the same scope merges in place
        let mut best: Option<(usize, f32)> = None;
        for (i, r) in db.records.iter().enumerate() {
            if r.level == level && r.project_id.as_deref() == project_id && !r.vector.is_empty() {
                let sim = cosine(&inc.vector, &r.vector);
                if sim > dup_threshold && best.map(|(_, s)| sim > s).unwrap_or(true) {
                    best = Some((i, sim));
                }
            }
        }
        let i = best?.0;
        let rec = &mut db.records[i];
        rec.text = inc.text.clone();
        rec.vector = inc.vector.clone();
        for (k, v) in inc.params.clone() {
            rec.params.insert(k, v);
        }
        rec.confidence = (1.0 - (1.0 - rec.confidence) * (1.0 - inc.confidence)).min(0.99);
        rec.last_used_at_ms = inc.now;
        rec.use_count += 1;
        if inc.topic.is_some() {
            rec.topic = inc.topic.clone();
        }
        Some(rec.id.clone())
    }

    /// Write-path step 3: allocate a fresh cache line for the payload.
    fn allocate_line(
        &self,
        db: &mut UserDb,
        user: &str,
        level: Level,
        project_id: Option<String>,
        group: Option<String>,
        kind: MemoryKind,
        inc: &Incoming,
    ) -> String {
        let id = self.next_id(&inc.text, inc.now);
        db.records.push(MemoryRecord {
            id: id.clone(),
            user_id: user.to_string(),
            text: inc.text.clone(),
            vector: inc.vector.clone(),
            level,
            project_id,
            group,
            topic: inc.topic.clone(),
            kind,
            params: inc.params.clone(),
            key_hint: inc.key_hint.clone(),
            confidence: inc.confidence,
            pinned: inc.pinned.unwrap_or(false),
            created_at_ms: inc.now,
            last_used_at_ms: inc.now,
            use_count: 0,
            origin: None,
            expires_at_ms: inc.expires_at_ms,
        });
        id
    }

    // -- projects ------------------------------------------------------------

    pub fn register_project(&self, req: ProjectInput) -> Result<ProjectInfo> {
        if req.project_id.trim().is_empty() {
            return Err(MemoryError::invalid("project_id must not be empty"));
        }
        let now = (self.config.now)();
        let descriptor = req.descriptor.clone().unwrap_or_else(|| {
            let mut parts = vec![req.name.clone().unwrap_or_else(|| req.project_id.clone())];
            parts.extend(req.tags.iter().cloned());
            parts.extend(req.components.iter().cloned());
            parts
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        });
        if descriptor.trim().is_empty() {
            return Err(MemoryError::invalid(
                "project needs a descriptor (name, tags, components or explicit descriptor text)",
            ));
        }
        let vector = self
            .embedder
            .embed(std::slice::from_ref(&descriptor))?
            .into_iter()
            .next()
            .expect("one embed");
        ensure_embedded(&vector, &format!("project descriptor `{descriptor}`"))?;

        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        // `none` records an explicit no-group confirmation; any other value
        // must be a path-safe group name. A request without `group` never
        // wipes an existing assignment (re-init / re-register keeps it).
        let group = match req.group.as_deref() {
            Some(NO_GROUP) => Some(NO_GROUP.to_string()),
            Some(g) => {
                if !crate::store::valid_path_segment(g) {
                    return Err(MemoryError::invalid(format!(
                        "invalid group name `{g}` (use letters, digits, '-', '_', '.')"
                    )));
                }
                Some(g.to_string())
            }
            None => None,
        };
        let group = group.or_else(|| {
            db.projects
                .get(&req.project_id)
                .and_then(|p| p.group.clone())
        });
        // re-registration never wipes user-managed `uses` links either
        let uses = db
            .projects
            .get(&req.project_id)
            .map(|p| p.uses.clone())
            .unwrap_or_default();
        db.projects.insert(
            req.project_id.clone(),
            ProjectInfo {
                project_id: req.project_id.clone(),
                name: req.name.clone().unwrap_or_else(|| req.project_id.clone()),
                tags: req.tags.clone(),
                components: req.components.clone(),
                descriptor,
                descriptor_vector: vector,
                similar: Vec::new(),
                uses,
                group,
                created_at_ms: now,
            },
        );
        recompute_similar(&mut db, self.config.similar_project_threshold);
        let info = db.projects[req.project_id.as_str()].clone();
        self.store.save(&req.user, &db)?;
        Ok(info)
    }

    pub fn list_projects(&self, user: &str) -> Result<Vec<ProjectInfo>> {
        let dba = self.user_db(user)?;
        let db = lock(&dba)?;
        Ok(db.projects.values().cloned().collect())
    }

    /// Assign the project's L2 group — the human-confirmed membership the
    /// skill asks about once per project. `Some("none")` records an explicit
    /// "belongs to no group" confirmation; `None` resets to unassigned.
    pub fn set_project_group(
        &self,
        user: &str,
        project_id: &str,
        group: Option<&str>,
    ) -> Result<ProjectInfo> {
        let group = match group {
            None => None,
            Some(NO_GROUP) => Some(NO_GROUP.to_string()),
            Some(g) => {
                if !crate::store::valid_path_segment(g) || g == NO_GROUP {
                    return Err(MemoryError::invalid(format!(
                        "invalid group name `{g}` (use letters, digits, '-', '_', '.')"
                    )));
                }
                Some(g.to_string())
            }
        };
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        let Some(p) = db.projects.get_mut(project_id) else {
            return Err(MemoryError::ProjectNotFound(project_id.to_string()));
        };
        p.group = group;
        let info = p.clone();
        self.store.save(user, &db)?;
        Ok(info)
    }

    /// Rename an L2 group everywhere at once: every project assigned to
    /// `from` and every group-owned memory carrying it move to `to`. When
    /// `to` is already an existing group this is a **merge** — the fix for
    /// accidentally split families (`rustcli` vs `rust-clis`). Returns
    /// (projects moved, group-owned memories moved).
    pub fn rename_group(&self, user: &str, from: &str, to: &str) -> Result<(usize, usize)> {
        if from == to {
            return Err(MemoryError::invalid(
                "from and to are the same group — nothing to rename",
            ));
        }
        if !crate::store::valid_path_segment(to) || to == NO_GROUP {
            return Err(MemoryError::invalid(format!(
                "invalid group name `{to}` (use letters, digits, '-', '_', '.')"
            )));
        }
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        let mut projects = 0;
        let mut records = 0;
        for p in db.projects.values_mut() {
            if p.group.as_deref() == Some(from) {
                p.group = Some(to.to_string());
                projects += 1;
            }
        }
        for r in db.records.iter_mut() {
            if r.group.as_deref() == Some(from) {
                r.group = Some(to.to_string());
                records += 1;
            }
        }
        if projects == 0 && records == 0 {
            return Err(MemoryError::invalid(format!(
                "no project or memory belongs to group `{from}`"
            )));
        }
        // the store reconciles the stale group's doc directory on save
        self.store.save(user, &db)?;
        Ok((projects, records))
    }

    /// Make `project_id` draw on `uses_id`: its L1 and L2 memories surface in
    /// `project_id` (from the warm L2 tier). Directional — only the using
    /// project gains visibility. Both projects must be registered; adding an
    /// existing link is a no-op.
    pub fn add_project_use(&self, user: &str, project_id: &str, uses_id: &str) -> Result<ProjectInfo> {
        self.edit_project_use(user, project_id, uses_id, true)
    }

    /// Remove a `uses` link added by [`MemoryEngine::add_project_use`].
    /// Removing a link that isn't there is a no-op.
    pub fn remove_project_use(
        &self,
        user: &str,
        project_id: &str,
        uses_id: &str,
    ) -> Result<ProjectInfo> {
        self.edit_project_use(user, project_id, uses_id, false)
    }

    fn edit_project_use(
        &self,
        user: &str,
        project_id: &str,
        uses_id: &str,
        add: bool,
    ) -> Result<ProjectInfo> {
        if uses_id == project_id {
            return Err(MemoryError::invalid(
                "a project cannot use itself — its own memories are already visible to it",
            ));
        }
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        for id in [project_id, uses_id] {
            if !db.projects.contains_key(id) {
                return Err(MemoryError::ProjectNotFound(id.to_string()));
            }
        }
        let p = db.projects.get_mut(project_id).expect("checked above");
        if add {
            if !p.uses.iter().any(|s| s == uses_id) {
                p.uses.push(uses_id.to_string());
            }
        } else {
            p.uses.retain(|s| s != uses_id);
        }
        let info = p.clone();
        self.store.save(user, &db)?;
        Ok(info)
    }

    /// Unregister a project: drop its registry entry, all of its records
    /// (every level), and any similarity links pointing at it. Returns the
    /// number of records removed. 404s when the project is unknown.
    pub fn remove_project(&self, user: &str, project_id: &str) -> Result<usize> {
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        if db.projects.remove(project_id).is_none() {
            return Err(MemoryError::ProjectNotFound(project_id.to_string()));
        }
        let before = db.records.len();
        db.records
            .retain(|r| r.project_id.as_deref() != Some(project_id));
        let removed = before - db.records.len();
        for p in db.projects.values_mut() {
            p.similar.retain(|s| s != project_id);
            p.uses.retain(|s| s != project_id);
        }
        // the store reconciles the project's L1 folder and links file on save
        self.store.save(user, &db)?;
        Ok(removed)
    }

    // -- write path ----------------------------------------------------------

    pub fn remember(&self, req: RememberInput) -> Result<RememberOutcome> {
        let text = req.text.trim().to_string();
        if text.is_empty() {
            return Err(MemoryError::invalid("text must not be empty"));
        }
        let kind = req.kind.unwrap_or({
            if req.key_hint.is_some() {
                MemoryKind::Feedback
            } else {
                MemoryKind::Preference
            }
        });
        let (level, project_id, group, topic) = resolve_placement(&req, kind)?;
        let mut incoming = Incoming::new(
            text,
            req.params.unwrap_or_default(),
            req.key_hint.clone(),
            req.confidence.unwrap_or(0.8).clamp(0.05, 1.0),
            req.ttl_days,
            req.pinned,
            topic,
            (self.config.now)(),
        )?;
        // embed before locking: this may be a slow HTTP call
        incoming.vector = self
            .embedder
            .embed(std::slice::from_ref(&incoming.text))?
            .into_iter()
            .next()
            .expect("one embed");
        ensure_embedded(&incoming.vector, &format!("memory `{}`", incoming.text))?;
        let now = incoming.now;

        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;

        // fold into an existing record at this scope (by parameter key, then
        // by near-identical content); otherwise allocate a fresh cache line
        let upserted = Self::upsert_in_scope(
            &mut db,
            level,
            project_id.as_deref(),
            &incoming,
            self.config.dup_threshold,
        );
        let deduped = upserted.is_some();
        let record_id = match upserted {
            Some(id) => id,
            None => self.allocate_line(
                &mut db, &req.user, level, project_id, group, kind, &incoming,
            ),
        };

        let demoted_to_l2 = self.enforce_capacity(&mut db, level, now);
        self.store.save(&req.user, &db)?;
        drop(db);

        let n = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        let auto_consolidated = self.config.auto_consolidate_every > 0
            && n.is_multiple_of(self.config.auto_consolidate_every);
        if auto_consolidated {
            self.consolidate(&req.user)?;
        }

        Ok(RememberOutcome {
            id: record_id,
            deduped,
            demoted_to_l2,
            auto_consolidated,
        })
    }

    pub fn feedback(&self, req: FeedbackInput) -> Result<RememberOutcome> {
        let global = req.global.unwrap_or(false);
        if !global && req.project_id.is_none() {
            return Err(MemoryError::invalid(
                "feedback needs a project_id, or global=true for a cross-project preference",
            ));
        }
        let text = format!("{} = {}", req.key, req.value.as_text());
        self.remember(RememberInput {
            user: req.user,
            text,
            project_id: if global { None } else { req.project_id },
            kind: Some(MemoryKind::Feedback),
            params: Some(BTreeMap::from([(req.key.clone(), req.value)])),
            key_hint: Some(req.key),
            confidence: Some(req.weight.unwrap_or(0.8).clamp(0.05, 1.0)),
            pinned: None,
            ttl_days: None,
            level: None,
            group: None,
            topic: None,
        })
    }

    // -- read path -----------------------------------------------------------

    pub fn recall(&self, req: RecallInput) -> Result<RecallOutput> {
        let query = req.query.trim().to_string();
        if query.is_empty() {
            return Err(MemoryError::invalid("query must not be empty"));
        }
        let now = (self.config.now)();
        let qvec = self
            .embedder
            .embed(std::slice::from_ref(&query))?
            .into_iter()
            .next()
            .expect("one embed");

        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;

        let k = req.k.unwrap_or(self.config.default_k).clamp(1, 100);
        let project = req.project_id.clone();
        let (similar, uses): (HashSet<String>, HashSet<String>) = match project
            .as_deref()
            .and_then(|p| db.projects.get(p))
        {
            Some(pi) => (
                pi.similar.iter().cloned().collect(),
                pi.uses.iter().cloned().collect(),
            ),
            None => (HashSet::new(), HashSet::new()),
        };
        let groups = effective_groups(&db);
        let my_group = project.as_deref().and_then(|p| groups.get(p).cloned());
        let min_sim = req
            .min_similarity
            .or(self.config.default_min_similarity)
            .unwrap_or_else(|| self.embedder.default_min_similarity());

        struct Cand {
            idx: usize,
            sim: f32,
            score: f32,
        }
        let mut cands: Vec<Cand> = Vec::new();
        for (idx, r) in db.records.iter().enumerate() {
            if r.is_expired(now) || r.vector.is_empty() {
                continue;
            }
            if !visible_at(
                r,
                project.as_deref(),
                &similar,
                my_group.as_deref(),
                &groups,
                &uses,
            ) {
                continue;
            }
            let sim = cosine(&qvec, &r.vector);
            if sim < min_sim {
                continue;
            }
            let score = recall_score(
                sim,
                r.serving_level_for(project.as_deref()),
                r.confidence,
                now.saturating_sub(r.last_used_at_ms.max(r.created_at_ms)),
            );
            cands.push(Cand { idx, sim, score });
        }
        cands.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut chosen: Vec<Cand> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for c in cands {
            let r = &db.records[c.idx];
            if !seen.insert(r.canonical_id().to_string()) {
                continue; // promoted copy of something already served
            }
            chosen.push(c);
            if chosen.len() >= k {
                break;
            }
        }

        // touch what we served (recency + usage feed eviction and promotion)
        for c in &chosen {
            let r = &mut db.records[c.idx];
            r.use_count += 1;
            r.last_used_at_ms = now;
        }

        // write-allocate: hot L2/L3 memories migrate up into L1
        let mut promoted: Vec<String> = Vec::new();
        let do_promote = req.write_allocate.unwrap_or(self.config.write_allocate);
        if do_promote {
            if let Some(pid) = &project {
                let hot: Vec<usize> = chosen
                    .iter()
                    .filter(|c| {
                        let r = &db.records[c.idx];
                        // borrowed L1 lines serve warm here, so they may
                        // promote into this project's hot line like any L2 hit
                        r.serving_level_for(Some(pid.as_str())) != Level::L1
                            && r.use_count >= self.config.promote_min_hits
                    })
                    .map(|c| c.idx)
                    .collect();
                for idx in hot {
                    if let Some(new_id) = self.promote_into_l1(&mut db, &req.user, pid, idx, now) {
                        promoted.push(new_id);
                    }
                }
                if !promoted.is_empty() {
                    self.enforce_capacity(&mut db, Level::L1, now);
                }
            }
        }

        let hits = chosen
            .iter()
            .map(|c| {
                let r = &db.records[c.idx];
                RecallHit {
                    id: r.id.clone(),
                    text: r.text.clone(),
                    similarity: c.sim,
                    score: c.score,
                    // the tier it served from, not where it lives — a used
                    // project's L1 lines surface here as warm (L2) hits
                    level: r.serving_level_for(project.as_deref()),
                    kind: r.kind,
                    project_id: r.project_id.clone(),
                    params: r.params.clone(),
                    key_hint: r.key_hint.clone(),
                    confidence: r.confidence,
                    use_count: r.use_count,
                    created_at_ms: r.created_at_ms,
                }
            })
            .collect();

        let mut searched = Vec::new();
        if project.is_some() {
            searched.push(Level::L1);
            searched.push(Level::L2);
        }
        searched.push(Level::L3);

        self.store.save(&req.user, &db)?;
        Ok(RecallOutput {
            hits,
            searched,
            promoted,
        })
    }

    // -- parameters ----------------------------------------------------------

    pub fn adjusted_parameters(
        &self,
        user: &str,
        project_id: Option<&str>,
    ) -> Result<Vec<ParamSuggestion>> {
        let now = (self.config.now)();
        let dba = self.user_db(user)?;
        let db = lock(&dba)?;
        let (similar, uses): (HashSet<String>, HashSet<String>) = match project_id
            .and_then(|p| db.projects.get(p))
        {
            Some(pi) => (
                pi.similar.iter().cloned().collect(),
                pi.uses.iter().cloned().collect(),
            ),
            None => (HashSet::new(), HashSet::new()),
        };
        let groups = effective_groups(&db);
        let my_group = project_id.and_then(|p| groups.get(p).cloned());
        let visible: Vec<&MemoryRecord> = db
            .records
            .iter()
            .filter(|r| {
                !r.is_expired(now)
                    && visible_at(
                        r,
                        project_id,
                        &similar,
                        my_group.as_deref(),
                        &groups,
                        &uses,
                    )
            })
            .collect();
        Ok(params::collect_suggestions(
            &visible,
            project_id,
            now,
            self.config.half_life_days,
            self.config.numeric_param_tolerance,
        ))
    }

    /// Merge defaults (the host app's baseline) with the learner's adjusted
    /// values — convenience so callers get one ready-to-apply map.
    pub fn parameters_with_defaults(
        &self,
        user: &str,
        project_id: Option<&str>,
        defaults: &BTreeMap<String, ParamValue>,
    ) -> Result<BTreeMap<String, ParamValue>> {
        let mut out = defaults.clone();
        for s in self.adjusted_parameters(user, project_id)? {
            out.insert(s.key, s.value);
        }
        Ok(out)
    }

    /// Everything currently held across the three layers for one project
    /// scope — the "gather" half of the sync pipeline and useful for prompt
    /// injection. L1: this project's hot lines. L2: related scopes (sibling
    /// components + similar projects). L3: user-level traits. Capped per
    /// layer (most recently used first) so prompts stay bounded.
    pub fn memory_context(
        &self,
        user: &str,
        project_id: Option<&str>,
        per_layer_cap: usize,
    ) -> Result<MemoryContext> {
        let now = (self.config.now)();
        let dba = self.user_db(user)?;
        let db = lock(&dba)?;
        let (similar, uses): (HashSet<String>, HashSet<String>) = match project_id
            .and_then(|p| db.projects.get(p))
        {
            Some(pi) => (
                pi.similar.iter().cloned().collect(),
                pi.uses.iter().cloned().collect(),
            ),
            None => (HashSet::new(), HashSet::new()),
        };
        let groups = effective_groups(&db);
        let my_group = project_id.and_then(|p| groups.get(p).cloned());
        let visible = |r: &MemoryRecord| {
            !r.is_expired(now)
                && visible_at(
                    r,
                    project_id,
                    &similar,
                    my_group.as_deref(),
                    &groups,
                    &uses,
                )
        };

        // bucket by the layer each record *serves at* for this project — a
        // used project's L1 lines are warm context here, not our hot lines
        let mut by_layer: BTreeMap<Level, Vec<MemoryLine>> = BTreeMap::new();
        for r in db.records.iter().filter(|r| visible(r)) {
            by_layer
                .entry(r.serving_level_for(project_id))
                .or_default()
                .push(MemoryLine {
                id: r.id.clone(),
                text: r.text.clone(),
                params: r.params.clone(),
                key_hint: r.key_hint.clone(),
                confidence: r.confidence,
                pinned: r.pinned,
            });
        }
        for lines in by_layer.values_mut() {
            lines.truncate(per_layer_cap.max(1));
        }
        let group_members = match (project_id, my_group.clone()) {
            (Some(p), Some(g)) => db
                .projects
                .iter()
                .filter(|(id, _)| {
                    id.as_str() != p && groups.get(*id).map(String::as_str) == Some(g.as_str())
                })
                .map(|(id, _)| id.clone())
                .collect(),
            _ => Vec::new(),
        };
        Ok(MemoryContext {
            l1: by_layer.remove(&Level::L1).unwrap_or_default(),
            l2: by_layer.remove(&Level::L2).unwrap_or_default(),
            l3: by_layer.remove(&Level::L3).unwrap_or_default(),
            params: params::collect_suggestions(
                &db.records.iter().filter(|r| visible(r)).collect::<Vec<_>>(),
                project_id,
                now,
                self.config.half_life_days,
                self.config.numeric_param_tolerance,
            ),
            group: my_group,
            group_members,
        })
    }

    // -- maintenance ---------------------------------------------------------

    pub fn consolidate(&self, user: &str) -> Result<ConsolidationReport> {
        let now = (self.config.now)();
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
        let mut report = ConsolidationReport::default();

        // 1) expiry (pinned memories survive)
        let before = db.records.len();
        db.records.retain(|r| !r.is_expired(now));
        report.expired = before - db.records.len();

        // 2) forgetting: below the confidence floor
        let before = db.records.len();
        db.records
            .retain(|r| r.pinned || r.confidence >= self.config.confidence_floor);
        report.forgotten = before - db.records.len();

        // 3) merge near-duplicates within the same (level, scope)
        let len = db.records.len();
        let mut removed = vec![false; len];
        for i in 0..len {
            if removed[i] {
                continue;
            }
            for j in (i + 1)..len {
                if removed[j] || removed[i] {
                    continue;
                }
                let key_conflict = db.records[i].key_hint.is_some()
                    && db.records[j].key_hint.is_some()
                    && db.records[i].key_hint != db.records[j].key_hint;
                if key_conflict
                    || db.records[i].level != db.records[j].level
                    || db.records[i].project_id != db.records[j].project_id
                {
                    continue;
                }
                let sim = cosine(&db.records[i].vector, &db.records[j].vector);
                if sim <= self.config.merge_threshold {
                    continue;
                }
                let (k, d) = if db.records[i].created_at_ms >= db.records[j].created_at_ms {
                    (i, j)
                } else {
                    (j, i)
                };
                let conf = (1.0
                    - (1.0 - db.records[k].confidence) * (1.0 - db.records[d].confidence))
                    .min(0.99);
                let extra = db.records[d].params.clone();
                let used = db.records[d].use_count;
                let last_used = db.records[d].last_used_at_ms;
                let kr = &mut db.records[k];
                kr.confidence = conf;
                for (kk, vv) in extra {
                    kr.params.entry(kk).or_insert(vv);
                }
                kr.use_count += used;
                kr.last_used_at_ms = kr.last_used_at_ms.max(last_used);
                removed[d] = true;
                report.merged += 1;
            }
        }
        if report.merged > 0 {
            let mut keep = Vec::with_capacity(db.records.len() - report.merged);
            for (i, r) in db.records.drain(..).enumerate() {
                if !removed[i] {
                    keep.push(r);
                }
            }
            db.records = keep;
        }

        // 4) trait lift: a parameter that keeps agreeing across projects is a
        //    global trait — lift it to L3 so every future project inherits it.
        report.traits_lifted += self.lift_agreeing_traits(&mut db, user, now)?;

        // 5) capacity across all layers
        for lvl in [Level::L1, Level::L2, Level::L3] {
            self.enforce_capacity(&mut db, lvl, now);
        }

        self.store.save(user, &db)?;
        Ok(report)
    }

    /// Consolidation step 4. For every parameter key asserted with a
    /// compatible value across `trait_lift_min_projects` distinct projects,
    /// upsert one global L3 trait carrying the consensus value.
    fn lift_agreeing_traits(&self, db: &mut UserDb, user: &str, now: u64) -> Result<usize> {
        // newest assertion per (key, project)
        let mut by_key: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        for (idx, r) in db.records.iter().enumerate() {
            if !matches!(r.level, Level::L1 | Level::L2)
                || r.project_id.is_none()
                || r.is_expired(now)
            {
                continue;
            }
            for key in r.params.keys() {
                let per_project = by_key.entry(key.clone()).or_default();
                match per_project.get_mut(r.project_id.as_deref().expect("checked")) {
                    Some(slot) => {
                        if db.records[*slot].created_at_ms < r.created_at_ms {
                            *slot = idx;
                        }
                    }
                    None => {
                        per_project.insert(r.project_id.clone().expect("checked"), idx);
                    }
                }
            }
        }

        let mut lifted = 0;
        for (key, per_project) in by_key {
            if per_project.len() < self.config.trait_lift_min_projects {
                continue;
            }
            let values: Vec<&ParamValue> = per_project
                .values()
                .map(|&i| &db.records[i].params[&key])
                .collect();
            let Some(consensus) =
                params::consensus_value(&values, self.config.numeric_param_tolerance)
            else {
                continue; // projects disagree — keep it local, let precedence decide
            };
            self.upsert_l3_trait(db, user, &key, consensus, per_project.len(), now)?;
            lifted += 1;
        }
        Ok(lifted)
    }

    /// Create or refresh the single L3 record carrying `key`'s consensus value.
    fn upsert_l3_trait(
        &self,
        db: &mut UserDb,
        user: &str,
        key: &str,
        consensus: ParamValue,
        n_projects: usize,
        now: u64,
    ) -> Result<()> {
        let confidence = (0.5 + 0.15 * n_projects as f32).min(0.97);
        let text = format!(
            "Across {} projects, the learner consistently sets {} = {}.",
            n_projects,
            key,
            consensus.as_text()
        );
        let vector = self
            .embedder
            .embed(std::slice::from_ref(&text))?
            .into_iter()
            .next()
            .expect("one embed");
        ensure_embedded(&vector, &format!("lifted trait for `{key}`"))?;
        let existing = db
            .records
            .iter_mut()
            .find(|r| r.level == Level::L3 && r.key_hint.as_deref() == Some(key));
        match existing {
            Some(r) => {
                r.text = text;
                r.vector = vector;
                r.params.insert(key.to_string(), consensus);
                r.confidence = confidence;
                r.last_used_at_ms = now;
                r.use_count += 1;
            }
            None => {
                let id = self.next_id(&text, now);
                let mut params = BTreeMap::new();
                params.insert(key.to_string(), consensus);
                db.records.push(MemoryRecord {
                    id,
                    user_id: user.to_string(),
                    text,
                    vector,
                    level: Level::L3,
                    project_id: None,
                    group: None,
                    topic: None,
                    kind: MemoryKind::Trait,
                    params,
                    key_hint: Some(key.to_string()),
                    confidence,
                    pinned: false,
                    created_at_ms: now,
                    last_used_at_ms: now,
                    use_count: 0,
                    origin: None,
                    expires_at_ms: None,
                });
            }
        }
        Ok(())
    }

    pub fn forget(&self, req: ForgetInput) -> Result<usize> {
        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        let before = db.records.len();
        if let Some(id) = &req.id {
            db.records
                .retain(|r| r.id != *id && r.origin.as_deref() != Some(id.as_str()));
            if before == db.records.len() {
                return Err(MemoryError::MemoryNotFound(id.clone()));
            }
        } else if req.all == Some(true) {
            db.records.clear();
        } else if req.project_id.is_some() || req.level.is_some() {
            db.records.retain(|r| {
                let proj_ok = req
                    .project_id
                    .as_deref()
                    .is_none_or(|p| r.project_id.as_deref() == Some(p));
                let lvl_ok = req.level.is_none_or(|l| r.level == l);
                !(proj_ok && lvl_ok)
            });
        } else {
            return Err(MemoryError::invalid(
                "forget needs `id`, `project_id`, `level`, or all=true",
            ));
        }
        let removed = before - db.records.len();
        self.store.save(&req.user, &db)?;
        Ok(removed)
    }

    /// Re-embed every memory and project descriptor with the current embedder.
    /// The escape hatch after switching backends or models.
    pub fn reindex(&self, user: &str) -> Result<usize> {
        let dba = self.user_db(user)?;
        let mut db = lock(&dba)?;
        let texts: Vec<String> = db.records.iter().map(|r| r.text.clone()).collect();
        let vectors = self.embedder.embed(&texts)?;
        // reindex overwrites every vector under the same fingerprint — a
        // broken embedder would brick the store silently, so refuse first
        for (text, v) in texts.iter().zip(&vectors) {
            ensure_embedded(v, &format!("reindex of `{text}`"))?;
        }
        for (r, v) in db.records.iter_mut().zip(vectors) {
            r.vector = v;
        }
        let descriptors: Vec<String> = db.projects.values().map(|p| p.descriptor.clone()).collect();
        let dvecs = self.embedder.embed(&descriptors)?;
        let pids: Vec<String> = db.projects.keys().cloned().collect();
        for (pid, v) in pids.into_iter().zip(dvecs) {
            if let Some(p) = db.projects.get_mut(&pid) {
                p.descriptor_vector = v;
            }
        }
        recompute_similar(&mut db, self.config.similar_project_threshold);
        db.embedder = self.embedder.fingerprint();
        db.dims = self.embedder.dims();
        let n = db.records.len();
        self.store.save(user, &db)?;
        Ok(n)
    }

    // -- introspection -------------------------------------------------------

    pub fn stats(&self, user: &str) -> Result<EngineStats> {
        let dba = self.user_db(user)?;
        let db = lock(&dba)?;
        let mut keys = BTreeSet::new();
        for r in &db.records {
            keys.extend(r.params.keys().cloned());
        }
        Ok(EngineStats {
            user: user.to_string(),
            embedder: db.embedder.clone(),
            dims: db.dims,
            counts: Counts {
                l1: db.count_in(Level::L1),
                l2: db.count_in(Level::L2),
                l3: db.count_in(Level::L3),
            },
            capacities: Counts {
                l1: self.config.l1_capacity,
                l2: self.config.l2_capacity,
                l3: self.config.l3_capacity,
            },
            projects: db.projects.len(),
            keys: keys.into_iter().collect(),
        })
    }

    pub fn health(&self) -> HealthInfo {
        let users = self.store.users().map(|u| u.len()).unwrap_or(0);
        HealthInfo {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            embedder: self.embedder.fingerprint(),
            dims: self.embedder.dims(),
            users,
        }
    }
}

/// project id → effective L2 group for every project that has one (`none`
/// filtered out). Built once per read path and handed to `visible_at`.
fn effective_groups(db: &UserDb) -> HashMap<String, String> {
    db.projects
        .iter()
        .filter_map(|(id, p)| {
            effective_group(p.group.as_deref()).map(|g| (id.clone(), g.to_string()))
        })
        .collect()
}

/// Resolve the write placement from a request: the explicit level (or the
/// automatic routing), group ownership, the project scope, and the normalized
/// topic slug. All placement errors are raised here so `remember` stays a
/// pure write path.
///
/// Group ownership is an L2 concept: `group` writes a record owned by the
/// whole family of projects ("all my CLIs use clap"). Otherwise an L2 record
/// is project-owned and its group visibility is derived from the project's
/// *current* assignment at read time — regrouping a project moves what its
/// memories surface to, no rewrite needed. For the same reason a group-owned
/// record never carries a project: its home is the group, and regrouping a
/// project must not drag group-level facts along.
fn resolve_placement(
    req: &RememberInput,
    kind: MemoryKind,
) -> Result<(Level, Option<String>, Option<String>, Option<String>)> {
    let level = req.level.unwrap_or({
        if kind == MemoryKind::Trait || req.project_id.is_none() {
            Level::L3
        } else {
            Level::L1
        }
    });
    let group =
        match req.group.as_deref() {
            None => None,
            Some(NO_GROUP) => return Err(MemoryError::invalid(
                "`none` is reserved (it confirms a project has no group) — not a writable group",
            )),
            Some(g) => {
                if level != Level::L2 {
                    return Err(MemoryError::invalid(
                        "group-owned memories are L2 — pass level=L2 together with `group`",
                    ));
                }
                if !crate::store::valid_path_segment(g) || g == NO_GROUP {
                    return Err(MemoryError::invalid(format!(
                        "invalid group name `{g}` (use letters, digits, '-', '_', '.')"
                    )));
                }
                Some(g.to_string())
            }
        };
    let project_id = if group.is_some() {
        None
    } else {
        req.project_id.clone()
    };
    if level == Level::L1 && project_id.is_none() {
        return Err(MemoryError::invalid("L1 memories require a project_id"));
    }
    if level == Level::L2 && project_id.is_none() && group.is_none() {
        return Err(MemoryError::invalid(
            "L2 memories need a project_id (project-owned) or a group (group-owned)",
        ));
    }
    let topic = req.topic.as_deref().and_then(crate::store::normalize_topic);
    Ok((level, project_id, group, topic))
}

/// The validated payload of a [`MemoryEngine::remember`] call — everything
/// the write path needs, independent of where it lands. The embedding is
/// attached right after construction so the (possibly slow) provider call
/// happens *before* the store lock is taken.
struct Incoming {
    text: String,
    vector: Vec<f32>,
    params: BTreeMap<String, ParamValue>,
    /// Canonical parameter key for upsert-by-key; falls back to the single
    /// asserted parameter.
    key_hint: Option<String>,
    confidence: f32,
    expires_at_ms: Option<u64>,
    pinned: Option<bool>,
    topic: Option<String>,
    now: u64,
}

impl Incoming {
    #[allow(clippy::too_many_arguments)]
    fn new(
        text: String,
        params: BTreeMap<String, ParamValue>,
        key_hint: Option<String>,
        confidence: f32,
        ttl_days: Option<f64>,
        pinned: Option<bool>,
        topic: Option<String>,
        now: u64,
    ) -> Result<Self> {
        let expires_at_ms = ttl_days.map(|d| now + (d.max(0.0) * DAY_MS as f64) as u64);
        let key_hint = key_hint.or_else(|| {
            if params.len() == 1 {
                params.keys().next().cloned()
            } else {
                None
            }
        });
        Ok(Incoming {
            text,
            vector: Vec::new(),
            params,
            key_hint,
            confidence,
            expires_at_ms,
            pinned,
            topic,
            now,
        })
    }
}

/// Pairwise descriptor-similarity linking: projects whose descriptors embed
/// close enough share L2 memories. Symmetric, recomputed on registration.
fn recompute_similar(db: &mut UserDb, threshold: f32) {
    let ids: Vec<String> = db.projects.keys().cloned().collect();
    for a in &ids {
        for b in &ids {
            if a == b {
                continue;
            }
            let (va, vb) = (
                &db.projects[a.as_str()].descriptor_vector,
                &db.projects[b.as_str()].descriptor_vector,
            );
            if !va.is_empty() && !vb.is_empty() && cosine(va, vb) >= threshold {
                let pa = &mut db.projects.get_mut(a).expect("exists");
                if !pa.similar.contains(b) {
                    pa.similar.push(b.clone());
                }
            }
        }
    }
}
