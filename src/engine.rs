//! The layered memory engine.
//!
//! Implements CPU-cache semantics for learner memory:
//!
//! * **Read path** (`recall`) probes L1 → L2 → L3, merges and ranks hits
//!   (similarity × layer weight × confidence × recency), deduplicates
//!   promoted copies against their canonical records, and *touches* what it
//!   served. Repeatedly-used deeper-layer memories are copied up into L1
//!   (write-allocate) so hot knowledge migrates toward the learner.
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

pub(crate) fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    m.lock()
        .map_err(|p| MemoryError::Storage(format!("lock poisoned: {p}")))
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

fn visible_at(r: &MemoryRecord, project: Option<&str>, similar: &HashSet<String>) -> bool {
    match r.level {
        // L1 serves only its own project — the hot line.
        Level::L1 => project.is_some() && r.project_id.as_deref() == project,
        // L2 serves its own project *and* related scopes (same project's other
        // components live under the same project id; similar projects are
        // linked in `ProjectInfo.similar`).
        Level::L2 => match (project, r.project_id.as_deref()) {
            (Some(p), Some(rp)) => rp == p || similar.contains(rp),
            _ => false,
        },
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

        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;
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
        let level = req.level.unwrap_or({
            if kind == MemoryKind::Trait || req.project_id.is_none() {
                Level::L3
            } else {
                Level::L1
            }
        });
        if level == Level::L1 && req.project_id.is_none() {
            return Err(MemoryError::invalid("L1 memories require a project_id"));
        }
        let confidence = req.confidence.unwrap_or(0.8).clamp(0.05, 1.0);
        let now = (self.config.now)();
        let expires_at_ms = req
            .ttl_days
            .map(|d| now + (d.max(0.0) * DAY_MS as f64) as u64);
        let params = req.params.unwrap_or_default();
        let key_hint = req.key_hint.clone().or_else(|| {
            if params.len() == 1 {
                params.keys().next().cloned()
            } else {
                None
            }
        });

        let vector = self
            .embedder
            .embed(std::slice::from_ref(&text))?
            .into_iter()
            .next()
            .expect("one embed");

        let dba = self.user_db(&req.user)?;
        let mut db = lock(&dba)?;
        self.ensure_fingerprint(&db)?;

        let mut deduped = false;
        let mut record_id: Option<String> = None;

        // 1) upsert by key: same parameter asserted again at the same scope
        //    is an update, not a new line of cache.
        if let Some(kh) = &key_hint {
            if let Some(rec) = db.records.iter_mut().find(|r| {
                r.level == level
                    && r.project_id == req.project_id
                    && r.key_hint.as_deref() == Some(kh.as_str())
            }) {
                rec.text = text.clone();
                rec.vector = vector.clone();
                for (k, v) in params.clone() {
                    rec.params.insert(k, v);
                }
                rec.confidence = (rec.confidence + confidence) / 2.0;
                rec.last_used_at_ms = now;
                rec.expires_at_ms = expires_at_ms;
                rec.use_count += 1;
                if let Some(p) = req.pinned {
                    rec.pinned = p;
                }
                deduped = true;
                record_id = Some(rec.id.clone());
            }
        }

        // 2) near-duplicate content at the same scope merges in place.
        if record_id.is_none() {
            let mut best: Option<(usize, f32)> = None;
            for (i, r) in db.records.iter().enumerate() {
                if r.level == level && r.project_id == req.project_id && !r.vector.is_empty() {
                    let sim = cosine(&vector, &r.vector);
                    if sim > self.config.dup_threshold && best.map(|(_, s)| sim > s).unwrap_or(true)
                    {
                        best = Some((i, sim));
                    }
                }
            }
            if let Some((i, _)) = best {
                let rec = &mut db.records[i];
                rec.text = text.clone();
                rec.vector = vector.clone();
                for (k, v) in params.clone() {
                    rec.params.insert(k, v);
                }
                rec.confidence = (1.0 - (1.0 - rec.confidence) * (1.0 - confidence)).min(0.99);
                rec.last_used_at_ms = now;
                rec.use_count += 1;
                deduped = true;
                record_id = Some(rec.id.clone());
            }
        }

        // 3) otherwise allocate a fresh line.
        if record_id.is_none() {
            let id = self.next_id(&text, now);
            let record = MemoryRecord {
                id: id.clone(),
                user_id: req.user.clone(),
                text: text.clone(),
                vector,
                level,
                project_id: req.project_id.clone(),
                kind,
                params,
                key_hint,
                confidence,
                pinned: req.pinned.unwrap_or(false),
                created_at_ms: now,
                last_used_at_ms: now,
                use_count: 0,
                origin: None,
                expires_at_ms,
            };
            record_id = Some(id);
            db.records.push(record);
        }

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
            id: record_id.expect("id set"),
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
        let similar: HashSet<String> = project
            .as_deref()
            .and_then(|p| db.projects.get(p))
            .map(|pi| pi.similar.iter().cloned().collect())
            .unwrap_or_default();
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
            if !visible_at(r, project.as_deref(), &similar) {
                continue;
            }
            let sim = cosine(&qvec, &r.vector);
            if sim < min_sim {
                continue;
            }
            let age_days =
                now.saturating_sub(r.last_used_at_ms.max(r.created_at_ms)) as f32 / DAY_MS as f32;
            let score = sim
                * r.level.weight()
                * (0.5 + 0.5 * r.confidence)
                * (1.0 + 0.15 * 2f32.powf(-age_days / 14.0));
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
                let to_promote: Vec<usize> = chosen
                    .iter()
                    .filter(|c| {
                        let r = &db.records[c.idx];
                        r.level != Level::L1 && r.use_count >= self.config.promote_min_hits
                    })
                    .map(|c| c.idx)
                    .collect();
                for idx in to_promote {
                    let (id, text, vector, kind, params, key_hint, confidence) = {
                        let src = &db.records[idx];
                        (
                            src.id.clone(),
                            src.text.clone(),
                            src.vector.clone(),
                            src.kind,
                            src.params.clone(),
                            src.key_hint.clone(),
                            src.confidence,
                        )
                    };
                    let already = db.records.iter().any(|r| {
                        r.level == Level::L1
                            && r.project_id.as_deref() == Some(pid.as_str())
                            && (r.id == id || r.origin.as_deref() == Some(id.as_str()))
                    });
                    if already {
                        continue;
                    }
                    let new_id = self.next_id(&text, now);
                    db.records.push(MemoryRecord {
                        id: new_id.clone(),
                        user_id: req.user.clone(),
                        text,
                        vector,
                        level: Level::L1,
                        project_id: Some(pid.clone()),
                        kind,
                        params,
                        key_hint,
                        confidence,
                        pinned: false,
                        created_at_ms: now,
                        last_used_at_ms: now,
                        use_count: 1,
                        origin: Some(id),
                        expires_at_ms: None,
                    });
                    promoted.push(new_id);
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
                    level: r.level,
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
        let similar: HashSet<String> = project_id
            .and_then(|p| db.projects.get(p))
            .map(|pi| pi.similar.iter().cloned().collect())
            .unwrap_or_default();
        let visible: Vec<&MemoryRecord> = db
            .records
            .iter()
            .filter(|r| !r.is_expired(now) && visible_at(r, project_id, &similar))
            .collect();
        Ok(params::collect_suggestions(
            &visible,
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
        let similar: HashSet<String> = project_id
            .and_then(|p| db.projects.get(p))
            .map(|pi| pi.similar.iter().cloned().collect())
            .unwrap_or_default();

        let mut by_layer: BTreeMap<Level, Vec<MemoryLine>> = BTreeMap::new();
        for r in db
            .records
            .iter()
            .filter(|r| !r.is_expired(now) && visible_at(r, project_id, &similar))
        {
            by_layer.entry(r.level).or_default().push(MemoryLine {
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
        Ok(MemoryContext {
            l1: by_layer.remove(&Level::L1).unwrap_or_default(),
            l2: by_layer.remove(&Level::L2).unwrap_or_default(),
            l3: by_layer.remove(&Level::L3).unwrap_or_default(),
            params: params::collect_suggestions(
                &db.records
                    .iter()
                    .filter(|r| !r.is_expired(now) && visible_at(r, project_id, &similar))
                    .collect::<Vec<_>>(),
                now,
                self.config.half_life_days,
                self.config.numeric_param_tolerance,
            ),
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
        for (key, per_project) in by_key {
            if per_project.len() < self.config.trait_lift_min_projects {
                continue;
            }
            let idxs: Vec<usize> = per_project.values().copied().collect();
            let values: Vec<&ParamValue> =
                idxs.iter().map(|&i| &db.records[i].params[&key]).collect();
            let Some(consensus) =
                params::consensus_value(&values, self.config.numeric_param_tolerance)
            else {
                continue; // projects disagree — keep it local, let precedence decide
            };
            let n = per_project.len();
            let confidence = (0.5 + 0.15 * n as f32).min(0.97);
            let text = format!(
                "Across {} projects, the learner consistently sets {} = {}.",
                n,
                key,
                consensus.as_text()
            );
            let vector = self
                .embedder
                .embed(std::slice::from_ref(&text))?
                .into_iter()
                .next()
                .expect("one embed");
            let existing = db
                .records
                .iter()
                .position(|r| r.level == Level::L3 && r.key_hint.as_deref() == Some(key.as_str()));
            match existing {
                Some(pos) => {
                    let r = &mut db.records[pos];
                    r.text = text;
                    r.vector = vector;
                    r.params.insert(key.clone(), consensus);
                    r.confidence = confidence;
                    r.last_used_at_ms = now;
                    r.use_count += 1;
                }
                None => {
                    let id = self.next_id(&text, now);
                    let mut p = BTreeMap::new();
                    p.insert(key.clone(), consensus);
                    db.records.push(MemoryRecord {
                        id,
                        user_id: user.to_string(),
                        text,
                        vector,
                        level: Level::L3,
                        project_id: None,
                        kind: MemoryKind::Trait,
                        params: p,
                        key_hint: Some(key),
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
            report.traits_lifted += 1;
        }

        // 5) capacity across all layers
        for lvl in [Level::L1, Level::L2, Level::L3] {
            self.enforce_capacity(&mut db, lvl, now);
        }

        self.store.save(user, &db)?;
        Ok(report)
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
