//! Persistence backends behind the [`MemoryStore`] trait:
//!
//! * [`LayeredDirStore`] — the default. Mirrors the cache hierarchy on disk:
//!   `cache/L1/<project>/`, `cache/L2/` (per-group, per-topic docs +
//!   `similar-projects.txt`), `cache/L3/`, plus the hand-editable
//!   `cache/uses.txt` for cross-project memory sources. JSON files are the
//!   machine-authoritative record; every layer also gets a human-readable
//!   `memories.md` mirror regenerated on each write.
//! * [`JsonFileStore`] — one JSON file per user; simple flat alternative.

use crate::error::{MemoryError, Result};
use crate::types::{Level, MemoryKind, MemoryRecord, ProjectInfo, UserDb};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Pluggable persistence backend. The engine only ever talks to a user-keyed
/// load/save API, so backends can range from files to SQLite to a network service.
pub trait MemoryStore: Send + Sync {
    fn load(&self, user: &str) -> Result<Option<UserDb>>;
    fn save(&self, user: &str, db: &UserDb) -> Result<()>;
    fn users(&self) -> Result<Vec<String>>;
}

/// The reserved single-user name: its data lives directly at `{root}/cache/…`
/// instead of `{root}/users/{user}/cache/…`, matching the standalone-binary layout.
pub const LOCAL_USER: &str = "local";

/// Default data root: `$HOME/tiered-memory` (falls back to `./tiered-memory`).
pub fn default_data_dir() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) if !home.trim().is_empty() => PathBuf::from(home).join("tiered-memory"),
        _ => PathBuf::from("./tiered-memory"),
    }
}

/// Path-safe identifier (used for user, project, and group directory/file names).
pub(crate) fn valid_path_segment(seg: &str) -> bool {
    !seg.is_empty()
        && !seg.starts_with('.')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Fallback topic for L2 records filed without one.
pub(crate) const DEFAULT_TOPIC: &str = "general";

/// Normalize a topic slug ("Writing Style" → `writing-style`). Returns `None`
/// when nothing usable remains — callers fall back to [`DEFAULT_TOPIC`].
pub(crate) fn normalize_topic(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    (!out.is_empty() && valid_path_segment(&out)).then_some(out)
}

// ---------------------------------------------------------------------------
// JsonFileStore — flat, one file per user
// ---------------------------------------------------------------------------

/// JSON files under a root directory: `{root}/{user}.json`.
pub struct JsonFileStore {
    root: PathBuf,
}

impl JsonFileStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .map_err(|e| MemoryError::Storage(format!("cannot create {}: {e}", root.display())))?;
        Ok(JsonFileStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, user: &str) -> Result<PathBuf> {
        if !valid_path_segment(user) {
            return Err(MemoryError::Storage(format!(
                "invalid user id `{user}` (allowed: letters, digits, '-', '_', '.'; no leading dot)"
            )));
        }
        Ok(self.root.join(format!("{user}.json")))
    }
}

impl MemoryStore for JsonFileStore {
    fn load(&self, user: &str) -> Result<Option<UserDb>> {
        let path = self.path_for(user)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let db: UserDb = serde_json::from_slice(&bytes).map_err(|e| {
                    MemoryError::Storage(format!("corrupt store file {}: {e}", path.display()))
                })?;
                Ok(Some(db))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemoryError::Storage(format!(
                "cannot read {}: {e}",
                path.display()
            ))),
        }
    }

    fn save(&self, user: &str, db: &UserDb) -> Result<()> {
        let path = self.path_for(user)?;
        atomic_write(&path, &serde_json::to_vec_pretty(db)?)?;
        Ok(())
    }

    fn users(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let entries =
            fs::read_dir(&self.root).map_err(|e| MemoryError::Storage(format!("read_dir: {e}")))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".json") {
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// LayeredDirStore — the cache hierarchy as directories
// ---------------------------------------------------------------------------

    /// Layout (root defaults to `~/tiered-memory`):
    ///
    /// ```text
    /// {root}/                              ← user `local` (the standalone default)
    ///   meta.json                          store version + embedder fingerprint
    ///   current-project                    CLI selection marker (project id)
    ///   projects/<project-id>.json         project descriptors
    ///   cache/
    ///     uses.txt                         project → project memory sources (hand-editable)
    ///     L1/<project-id>/memories.json    hot, project-scoped records (machine)
    ///     L1/<project-id>/memories.md      …human-readable mirror
    ///     L2/memories.json                 related-scope records (flat machine store)
    ///     L2/groups/<group>/<topic>.md     human-readable docs per group + topic
    ///     L2/ungrouped/<topic>.md          …for projects without a group
    ///     L2/groups.txt                    project → group membership (hand-editable)
    ///     L2/similar-projects.txt          project similarity links (hand-editable)
    ///     L3/memories.json|md              user-level traits common to all projects
    ///   users/<other-user>/…                additional users (server mode)
    /// ```
    ///
    /// The JSON files are authoritative (ids, vectors, timestamps are not
    /// human-editable); the `.md` files are regenerated mirrors — read, grep and
    /// diff them, but edit through the API/CLI. The relationship files are the
    /// exception: hand-added lines in `similar-projects.txt` and `uses.txt`
    /// survive automatic recomputation (`groups.txt` wins on load outright).
pub struct LayeredDirStore {
    root: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    version: u32,
    embedder: String,
    dims: usize,
}

impl LayeredDirStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .map_err(|e| MemoryError::Storage(format!("cannot create {}: {e}", root.display())))?;
        Ok(LayeredDirStore { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Data directory for one user: `{root}` for `local`, `{root}/users/{u}` otherwise.
    pub fn user_dir(&self, user: &str) -> Result<PathBuf> {
        if !valid_path_segment(user) {
            return Err(MemoryError::Storage(format!(
                "invalid user id `{user}` (allowed: letters, digits, '-', '_', '.'; no leading dot)"
            )));
        }
        Ok(if user == LOCAL_USER {
            self.root.clone()
        } else {
            self.root.join("users").join(user)
        })
    }

    fn meta_path(&self, user: &str) -> Result<PathBuf> {
        Ok(self.user_dir(user)?.join("meta.json"))
    }

    fn current_project_path(&self, user: &str) -> Result<PathBuf> {
        Ok(self.user_dir(user)?.join("current-project"))
    }

    /// The CLI `select` marker: the project id currently being worked on, if set.
    pub fn current_project(&self, user: &str) -> Result<Option<String>> {
        match fs::read_to_string(self.current_project_path(user)?) {
            Ok(s) => Ok(Some(s.trim().to_string()).filter(|s| !s.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemoryError::Storage(format!("read current-project: {e}"))),
        }
    }

    pub fn set_current_project(&self, user: &str, project_id: &str) -> Result<()> {
        if !valid_path_segment(project_id) {
            return Err(MemoryError::invalid("invalid project id"));
        }
        atomic_write(
            &self.current_project_path(user)?,
            format!("{project_id}\n").as_bytes(),
        )
    }

    // -- layout helpers ------------------------------------------------------

    fn layer_dir(user_dir: &Path, level: Level) -> PathBuf {
        match level {
            Level::L1 => user_dir.join("cache").join("L1"),
            Level::L2 => user_dir.join("cache").join("L2"),
            Level::L3 => user_dir.join("cache").join("L3"),
        }
    }

    fn read_records(path: &Path) -> Result<Vec<MemoryRecord>> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| MemoryError::Storage(format!("corrupt {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(MemoryError::Storage(format!(
                "read {}: {e}",
                path.display()
            ))),
        }
    }

    fn write_records(
        dir: &Path,
        title: &str,
        embedder: &str,
        records: &[&MemoryRecord],
    ) -> Result<()> {
        fs::create_dir_all(dir)
            .map_err(|e| MemoryError::Storage(format!("create {}: {e}", dir.display())))?;
        atomic_write(
            &dir.join("memories.json"),
            &serde_json::to_vec_pretty(records)?,
        )?;
        atomic_write(
            &dir.join("memories.md"),
            render_md(title, embedder, records).as_bytes(),
        )?;
        Ok(())
    }

    /// JSON-only write (used for L2, whose human-readable layer lives in the
    /// per-group, per-topic MD files instead of one `memories.md`).
    fn write_json(dir: &Path, records: &[&MemoryRecord]) -> Result<()> {
        fs::create_dir_all(dir)
            .map_err(|e| MemoryError::Storage(format!("create {}: {e}", dir.display())))?;
        atomic_write(
            &dir.join("memories.json"),
            &serde_json::to_vec_pretty(records)?,
        )?;
        Ok(())
    }

    // -- per-section writers (used by save) -----------------------------------

    /// `projects/<id>.json` descriptors: write current, drop stale files.
    fn write_project_registry(user_dir: &Path, db: &UserDb) -> Result<()> {
        let projects_dir = user_dir.join("projects");
        fs::create_dir_all(&projects_dir)
            .map_err(|e| MemoryError::Storage(format!("create projects dir: {e}")))?;
        for (id, info) in &db.projects {
            if !valid_path_segment(id) {
                return Err(MemoryError::Storage(format!(
                    "project id `{id}` is not path-safe"
                )));
            }
            atomic_write(
                &projects_dir.join(format!("{id}.json")),
                &serde_json::to_vec_pretty(info)?,
            )?;
        }
        for entry in fs::read_dir(&projects_dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(stem) = name.strip_suffix(".json") {
                if !db.projects.contains_key(stem) {
                    fs::remove_file(entry.path())
                        .map_err(|e| MemoryError::Storage(format!("remove stale project: {e}")))?;
                }
            }
        }
        Ok(())
    }

    /// L1: one folder per registered/used project; directories of removed
    /// projects are reconciled away.
    fn write_l1_layer(user_dir: &Path, db: &UserDb) -> Result<()> {
        let mut by_project: BTreeMap<String, Vec<&MemoryRecord>> = BTreeMap::new();
        for r in db.records.iter().filter(|r| r.level == Level::L1) {
            if let Some(pid) = &r.project_id {
                by_project.entry(pid.clone()).or_default().push(r);
            }
        }
        // registered projects get a folder even with no records (the tree
        // mirrors the registry)
        let mut keep_dirs: Vec<String> = by_project.keys().cloned().collect();
        for id in db.projects.keys() {
            if !keep_dirs.contains(id) {
                keep_dirs.push(id.clone());
            }
        }

        let l1_root = Self::layer_dir(user_dir, Level::L1);
        fs::create_dir_all(&l1_root)?;
        for id in &keep_dirs {
            let empty: Vec<&MemoryRecord> = Vec::new();
            let records: &[&MemoryRecord] = match by_project.get(id) {
                Some(v) => v,
                None => &empty,
            };
            let dir = l1_root.join(id);
            let title = match db.projects.get(id) {
                Some(p) if !p.name.is_empty() => format!("L1 · {} ({id})", p.name),
                _ => format!("L1 · {id}"),
            };
            Self::write_records(&dir, &title, &db.embedder, records)?;
        }
        for entry in fs::read_dir(&l1_root)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if valid_path_segment(&name) && !keep_dirs.contains(&name) {
                    fs::remove_dir_all(&path).map_err(|e| {
                        MemoryError::Storage(format!("remove stale L1 dir {name}: {e}"))
                    })?;
                }
            }
        }
        Ok(())
    }

    /// L2: the flat machine-authoritative JSON plus the human-readable docs —
    /// no one big file, but per-group and per-topic MDs
    /// (`groups/<group>/<topic>.md`, e.g. `groups/rust-clis/writing-style.md`;
    /// records of groupless projects land in `ungrouped/<topic>.md` so every
    /// L2 record is rendered exactly once) — and the hand-editable
    /// `groups.txt` membership file (it wins on load).
    fn write_l2_layer(user_dir: &Path, db: &UserDb) -> Result<()> {
        let l2_dir = Self::layer_dir(user_dir, Level::L2);
        let l2: Vec<&MemoryRecord> = db.records.iter().filter(|r| r.level == Level::L2).collect();
        Self::write_json(&l2_dir, &l2)?;
        Self::write_l2_docs(&l2_dir, db, &l2)?;
        Self::write_groups_file(&l2_dir, db)?;
        Ok(())
    }

    /// Bucket L2 records by (group, topic) and regenerate the MD mirrors.
    fn write_l2_docs(l2_dir: &Path, db: &UserDb, l2: &[&MemoryRecord]) -> Result<()> {
        let mut buckets: BTreeMap<Option<String>, BTreeMap<String, Vec<&MemoryRecord>>> =
            BTreeMap::new();
        for r in l2 {
            let topic = r
                .topic
                .as_deref()
                .and_then(normalize_topic)
                .unwrap_or_else(|| DEFAULT_TOPIC.to_string());
            buckets
                .entry(record_group(r, &db.projects))
                .or_default()
                .entry(topic)
                .or_default()
                .push(r);
        }

        let groups_root = l2_dir.join("groups");
        fs::create_dir_all(&groups_root)
            .map_err(|e| MemoryError::Storage(format!("create {}: {e}", groups_root.display())))?;
        for (group, topics) in &buckets {
            let (dir, scope) = match group {
                Some(g) => (groups_root.join(g), format!("group {g}")),
                None => (l2_dir.join("ungrouped"), String::from("ungrouped")),
            };
            fs::create_dir_all(&dir)
                .map_err(|e| MemoryError::Storage(format!("create {}: {e}", dir.display())))?;
            for (topic, records) in topics {
                let title = format!("L2 · {scope} · {topic}");
                atomic_write(
                    &dir.join(format!("{topic}.md")),
                    render_md(&title, &db.embedder, records).as_bytes(),
                )?;
            }
            // the mirrors are fully generated: drop topic files whose records
            // all moved away
            for entry in fs::read_dir(&dir)?.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(stem) = name.strip_suffix(".md") {
                    if !topics.contains_key(stem) {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        for entry in fs::read_dir(&groups_root)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if valid_path_segment(&name) && !buckets.contains_key(&Some(name.clone())) {
                    fs::remove_dir_all(&path).map_err(|e| {
                        MemoryError::Storage(format!("remove stale group dir {name}: {e}"))
                    })?;
                }
            }
        }
        let ungrouped_dir = l2_dir.join("ungrouped");
        if !buckets.contains_key(&None) && ungrouped_dir.is_dir() {
            fs::remove_dir_all(&ungrouped_dir)
                .map_err(|e| MemoryError::Storage(format!("remove stale ungrouped dir: {e}")))?;
        }
        // migration: pre-topic single-file L2 mirrors are no longer written
        let _ = fs::remove_file(l2_dir.join("memories.md"));
        Ok(())
    }

    /// `groups.txt`: current assignments ∪ hand-added entries for projects
    /// that aren't registered (yet). For registered projects the db is
    /// authoritative here — clear an assignment with `none`, not by deleting
    /// the line (that just falls back to the project JSON).
    fn write_groups_file(l2_dir: &Path, db: &UserDb) -> Result<()> {
        let groups_path = l2_dir.join("groups.txt");
        let known: HashSet<&str> = db.projects.keys().map(|s| s.as_str()).collect();
        let mut entries: BTreeMap<String, String> = db
            .projects
            .iter()
            .filter_map(|(id, p)| p.group.clone().map(|g| (id.clone(), g)))
            .collect();
        for (pid, g) in read_groups_file(&groups_path)? {
            if !known.contains(pid.as_str()) {
                entries.entry(pid).or_insert(g);
            }
        }
        let mut text = String::from(
            "# tiered-memory · L2 group membership\n\
             # one per line: <project-id> <group> — hand edits win on load\n\
             # the reserved group `none` marks a project confirmed to have no group\n",
        );
        for (pid, g) in &entries {
            text.push_str(&format!("{pid} {g}\n"));
        }
        atomic_write(&groups_path, text.as_bytes())?;
        Ok(())
    }

    /// L3: global traits, one JSON + MD pair.
    fn write_l3_layer(user_dir: &Path, db: &UserDb) -> Result<()> {
        let l3: Vec<&MemoryRecord> = db.records.iter().filter(|r| r.level == Level::L3).collect();
        Self::write_records(
            &Self::layer_dir(user_dir, Level::L3),
            "L3 · learner traits (all projects)",
            &db.embedder,
            &l3,
        )
    }

    /// `similar-projects.txt`: existing hand-edits ∪ computed links.
    fn write_similar_links(user_dir: &Path, db: &UserDb) -> Result<()> {
        let link_path = Self::layer_dir(user_dir, Level::L2).join("similar-projects.txt");
        let mut pairs = read_link_file(&link_path)?;
        for p in db.projects.values() {
            for other in &p.similar {
                let mut pair = [p.project_id.clone(), other.clone()];
                pair.sort();
                let pair = (pair[0].clone(), pair[1].clone());
                if !pairs.contains(&pair) {
                    pairs.push(pair);
                }
            }
        }
        pairs.sort();
        pairs.dedup();
        let mut text = String::from(
            "# tiered-memory · L2 project similarity links\n\
             # one pair per line: <project-a> <project-b> (symmetric, order ignored)\n\
             # hand-added pairs are preserved across automatic recomputation\n",
        );
        for (a, b) in &pairs {
            text.push_str(&format!("{a} {b}\n"));
        }
        atomic_write(&link_path, text.as_bytes())?;
        Ok(())
    }

    /// `cache/uses.txt`: cross-project memory sources — `<using> <used>`,
    /// directional. Current links ∪ hand-added lines whose using-project is
    /// not registered (yet); for registered projects the db is authoritative
    /// and the CLI (`tiered-memory use --remove`) is how links are removed.
    fn write_uses_file(user_dir: &Path, db: &UserDb) -> Result<()> {
        let uses_path = user_dir.join("cache").join("uses.txt");
        let known: HashSet<&str> = db.projects.keys().map(|s| s.as_str()).collect();
        let mut entries: BTreeMap<String, BTreeSet<String>> = db
            .projects
            .iter()
            .filter(|(_, p)| !p.uses.is_empty())
            .map(|(id, p)| (id.clone(), p.uses.iter().cloned().collect()))
            .collect();
        for (using, used) in read_uses_file(&uses_path)? {
            if !known.contains(using.as_str()) {
                entries.entry(using).or_default().insert(used);
            }
        }
        let mut text = String::from(
            "# tiered-memory · cross-project memory sources\n\
             # one per line: <using-project> <used-project> (directional —\n\
             # `b a` means b sees a's L1+L2 memories in its warm tier)\n\
             # hand-added lines survive; remove links with `tiered-memory use --remove <project>`\n",
        );
        for (using, useds) in &entries {
            for used in useds {
                text.push_str(&format!("{using} {used}\n"));
            }
        }
        atomic_write(&uses_path, text.as_bytes())?;
        Ok(())
    }

    // -- per-section readers (used by load) -----------------------------------

    /// `meta.json`, or `None` when the store has never been written.
    fn read_meta(&self, user: &str) -> Result<Option<Meta>> {
        let meta_path = self.meta_path(user)?;
        match fs::read(&meta_path) {
            Ok(bytes) => {
                let meta: Meta = serde_json::from_slice(&bytes).map_err(|e| {
                    MemoryError::Storage(format!("corrupt {}: {e}", meta_path.display()))
                })?;
                Ok(Some(meta))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(MemoryError::Storage(format!(
                "read {}: {e}",
                meta_path.display()
            ))),
        }
    }

    /// `projects/<id>.json` descriptors.
    fn read_project_registry(user_dir: &Path, db: &mut UserDb) -> Result<()> {
        let projects_dir = user_dir.join("projects");
        let Ok(entries) = fs::read_dir(&projects_dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|e| {
                MemoryError::Storage(format!("read {}: {e}", entry.path().display()))
            })?;
            let p: ProjectInfo = serde_json::from_slice(&bytes).map_err(|e| {
                MemoryError::Storage(format!(
                    "corrupt project file {}: {e}",
                    entry.path().display()
                ))
            })?;
            db.projects.insert(p.project_id.clone(), p);
        }
        Ok(())
    }

    /// L1 — one folder per project. Directories without a registry entry
    /// become skeleton projects, so unregistered-but-used projects appear.
    fn read_l1_layer(user_dir: &Path, db: &mut UserDb) -> Result<()> {
        let l1_root = Self::layer_dir(user_dir, Level::L1);
        let Ok(entries) = fs::read_dir(&l1_root) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let project_id = entry.file_name().to_string_lossy().to_string();
            let records = Self::read_records(&entry.path().join("memories.json"))?;
            db.records.extend(records);
            db.projects
                .entry(project_id.clone())
                .or_insert_with(|| skeleton_project(&project_id));
        }
        Ok(())
    }

    /// L2/L3: records live in one flat `memories.json` per layer.
    fn read_flat_layer(user_dir: &Path, level: Level, db: &mut UserDb) -> Result<()> {
        let path = Self::layer_dir(user_dir, level).join("memories.json");
        let records = Self::read_records(&path)?;
        db.records.extend(records);
        Ok(())
    }

    /// Similarity links: the text file is authoritative and merges over JSON.
    /// Pairs referencing a not-yet-registered project stay attached to the
    /// known side and activate once that project registers.
    fn merge_similar_links(user_dir: &Path, db: &mut UserDb) -> Result<()> {
        let path = Self::layer_dir(user_dir, Level::L2).join("similar-projects.txt");
        for (a, b) in read_link_file(&path)? {
            attach_similar(db, &a, &b);
            attach_similar(db, &b, &a);
        }
        Ok(())
    }

    /// Groups: the text file is authoritative over project JSONs (hand edits
    /// win on load). Entries for not-yet-registered projects are ignored here
    /// but preserved on save, activating once they register.
    fn merge_group_membership(user_dir: &Path, db: &mut UserDb) -> Result<()> {
        let groups_path = Self::layer_dir(user_dir, Level::L2).join("groups.txt");
        for (pid, g) in read_groups_file(&groups_path)? {
            if let Some(p) = db.projects.get_mut(&pid) {
                p.group = Some(g);
            }
        }
        Ok(())
    }

    /// `uses.txt` merges into the project registry like the links file:
    /// hand-added lines attach to their using-project when it is registered
    /// and stay in the file until then.
    fn merge_uses(user_dir: &Path, db: &mut UserDb) -> Result<()> {
        let uses_path = user_dir.join("cache").join("uses.txt");
        for (using, used) in read_uses_file(&uses_path)? {
            if let Some(p) = db.projects.get_mut(&using) {
                if !p.uses.iter().any(|s| s == &used) {
                    p.uses.push(used);
                }
            }
        }
        Ok(())
    }
}

impl MemoryStore for LayeredDirStore {
    /// Rebuild the in-memory database from the directory tree. Each section
    /// has its own reader, mirroring the per-section writers in [`save`](Self::save):
    /// meta → project registry → L1 folders → L2/L3 JSON → similarity links →
    /// group membership. Missing files read as empty; only corruption is fatal.
    fn load(&self, user: &str) -> Result<Option<UserDb>> {
        let Some(meta) = self.read_meta(user)? else {
            return Ok(None); // no meta yet — a fresh store
        };
        let user_dir = self.user_dir(user)?;
        let mut db = UserDb::new(meta.embedder, meta.dims);

        Self::read_project_registry(&user_dir, &mut db)?;
        Self::read_l1_layer(&user_dir, &mut db)?;
        Self::read_flat_layer(&user_dir, Level::L2, &mut db)?;
        Self::read_flat_layer(&user_dir, Level::L3, &mut db)?;
        Self::merge_similar_links(&user_dir, &mut db)?;
        Self::merge_group_membership(&user_dir, &mut db)?;
        Self::merge_uses(&user_dir, &mut db)?;

        Ok(Some(db))
    }

    /// Persist the whole user database. Each layer/section has its own writer
    /// so this stays a readable map of the on-disk layout:
    /// meta → project registry → L1 folders → L2 (flat JSON + per-topic docs
    /// + membership file) → L3 → similarity links.
    fn save(&self, user: &str, db: &UserDb) -> Result<()> {
        let user_dir = self.user_dir(user)?;
        fs::create_dir_all(user_dir.join("cache"))
            .map_err(|e| MemoryError::Storage(format!("create cache dir: {e}")))?;

        atomic_write(
            &self.meta_path(user)?,
            &serde_json::to_vec_pretty(&Meta {
                version: db.version,
                embedder: db.embedder.clone(),
                dims: db.dims,
            })?,
        )?;
        Self::write_project_registry(&user_dir, db)?;
        Self::write_l1_layer(&user_dir, db)?;
        Self::write_l2_layer(&user_dir, db)?;
        Self::write_l3_layer(&user_dir, db)?;
        Self::write_similar_links(&user_dir, db)?;
        Self::write_uses_file(&user_dir, db)?;
        Ok(())
    }

    fn users(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if self.root.join("cache").is_dir() {
            out.push(LOCAL_USER.to_string());
        }
        let users_dir = self.root.join("users");
        if let Ok(entries) = fs::read_dir(&users_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    out.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

fn skeleton_project(project_id: &str) -> ProjectInfo {
    ProjectInfo {
        project_id: project_id.to_string(),
        name: project_id.to_string(),
        tags: Vec::new(),
        components: Vec::new(),
        descriptor: project_id.to_string(),
        descriptor_vector: Vec::new(),
        similar: Vec::new(),
        uses: Vec::new(),
        group: None,
        created_at_ms: crate::engine::system_now_ms(),
    }
}

fn read_link_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(MemoryError::Storage(format!(
                "read {}: {e}",
                path.display()
            )))
        }
    };
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(a), Some(b)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        if !valid_path_segment(a) || !valid_path_segment(b) || a == b {
            continue;
        }
        let mut pair = [a.to_string(), b.to_string()];
        pair.sort();
        pairs.push((pair[0].clone(), pair[1].clone()));
    }
    pairs.sort();
    pairs.dedup();
    Ok(pairs)
}

/// The L2 mirror bucket for one record: its explicit group, else its owning
/// project's group, else `None` (rendered under `ungrouped/`). Shared by the
/// store's own save() and the console's group views.
pub(crate) fn record_group(
    r: &MemoryRecord,
    projects: &BTreeMap<String, ProjectInfo>,
) -> Option<String> {
    if let Some(g) = r.group.as_deref().filter(|g| *g != crate::engine::NO_GROUP) {
        return Some(g.to_string());
    }
    r.project_id
        .as_deref()
        .and_then(|pid| projects.get(pid))
        .and_then(|p| p.group.as_deref())
        .filter(|g| *g != crate::engine::NO_GROUP)
        .map(|g| g.to_string())
}

/// Attach `other` to `project`'s similar list if that project is registered
/// (unregistered ids have no side to hold the link yet).
fn attach_similar(db: &mut UserDb, project: &str, other: &str) {
    if let Some(p) = db.projects.get_mut(project) {
        if !p.similar.iter().any(|s| s == other) {
            p.similar.push(other.to_string());
        }
    }
}

fn read_groups_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(MemoryError::Storage(format!(
                "read {}: {e}",
                path.display()
            )))
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(pid), Some(g)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        // `none` is a meaningful value (explicit no-group confirmation)
        if !valid_path_segment(pid) || !valid_path_segment(g) {
            continue;
        }
        out.push((pid.to_string(), g.to_string()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `cache/uses.txt` reader — `<using> <used>` directional pairs; self-pairs
/// and malformed ids are skipped. Sorted + deduped like the other link files.
fn read_uses_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(MemoryError::Storage(format!(
                "read {}: {e}",
                path.display()
            )))
        }
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        let (Some(using), Some(used)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        if !valid_path_segment(using) || !valid_path_segment(used) || using == used {
            continue;
        }
        out.push((using.to_string(), used.to_string()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Human-readable mirror of one layer's records. Regenerated on every write —
/// the JSON beside it is the machine-authoritative copy.
fn render_md(title: &str, embedder: &str, records: &[&MemoryRecord]) -> String {
    let mut out = format!(
        "# {title}\n\n> {} memories · embedder `{}` · regenerated {}\n",
        records.len(),
        embedder,
        fmt_utc(crate::engine::system_now_ms()),
    );
    if records.is_empty() {
        out.push_str("\n_(empty)_\n");
        return out;
    }
    let mut sorted: Vec<&MemoryRecord> = records.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.last_used_at_ms.max(r.created_at_ms)));
    for r in sorted {
        let kind = match r.kind {
            MemoryKind::Preference => "preference",
            MemoryKind::Trait => "trait",
            MemoryKind::Feedback => "feedback",
            MemoryKind::Summary => "summary",
            MemoryKind::Note => "note",
        };
        let pinned = if r.pinned { " · 📌 pinned" } else { "" };
        out.push_str(&format!(
            "\n## `{}` · {kind} · confidence {:.2} · used {}× · {}{pinned}\n\n{}\n",
            r.id,
            r.confidence,
            r.use_count,
            fmt_utc(r.last_used_at_ms.max(r.created_at_ms)),
            r.text.trim(),
        ));
        if !r.params.is_empty() {
            out.push_str("\n| parameter | value |\n|---|---|\n");
            for (k, v) in &r.params {
                out.push_str(&format!("| `{k}` | `{}` |\n", v.as_text()));
            }
        }
        if let Some(project) = &r.project_id {
            out.push_str(&format!("\n_project: `{project}`_\n"));
        }
    }
    out
}

/// Epoch-ms → `YYYY-MM-DD HH:MM UTC` (proleptic Gregorian, no deps).
fn fmt_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m) = ((secs % 86400) / 3600, (secs % 3600) / 60);
    let z = (secs / 86400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02} UTC")
}

/// Write via tmp file + rename so a crash never leaves a half-written store.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| MemoryError::Storage(format!("create {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)
        .map_err(|e| MemoryError::Storage(format!("write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, path)
        .map_err(|e| MemoryError::Storage(format!("rename into {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryKind;

    fn record(level: Level, project: Option<&str>, text: &str) -> MemoryRecord {
        MemoryRecord {
            id: crate::vector::gen_id("m", text, 42, 1),
            user_id: "u".into(),
            text: text.into(),
            vector: vec![0.5],
            level,
            project_id: project.map(|p| p.into()),
            group: None,
            topic: None,
            kind: MemoryKind::Preference,
            params: BTreeMap::new(),
            key_hint: None,
            confidence: 0.8,
            pinned: false,
            created_at_ms: 1_790_000_000_000,
            last_used_at_ms: 1_790_000_000_000,
            use_count: 0,
            origin: None,
            expires_at_ms: None,
        }
    }

    #[test]
    fn layered_roundtrip_and_layout() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();
        assert!(store.load(LOCAL_USER).unwrap().is_none());

        let mut db = UserDb::new("hashing:512".into(), 512);
        db.projects.insert(
            "teacher".into(),
            ProjectInfo {
                project_id: "teacher".into(),
                name: "Teacher".into(),
                tags: vec![],
                components: vec!["frontend".into()],
                descriptor: "tutoring app".into(),
                descriptor_vector: vec![],
                similar: vec!["music".into()],
                uses: vec![],
                group: None,
                created_at_ms: 1,
            },
        );
        db.projects.insert(
            "music".into(),
            ProjectInfo {
                project_id: "music".into(),
                name: "Music".into(),
                tags: vec![],
                components: vec![],
                descriptor: "piano".into(),
                descriptor_vector: vec![],
                similar: vec!["teacher".into()],
                uses: vec![],
                group: None,
                created_at_ms: 1,
            },
        );
        db.records
            .push(record(Level::L1, Some("teacher"), "hot line"));
        db.records
            .push(record(Level::L2, Some("teacher"), "warm line"));
        db.records.push(record(Level::L3, None, "global trait"));
        store.save(LOCAL_USER, &db).unwrap();

        // layout: L1 per project, L2 flat JSON + per-topic docs, L3 user-level
        let root = dir.path();
        assert!(root.join("cache/L1/teacher/memories.json").is_file());
        assert!(root.join("cache/L1/teacher/memories.md").is_file());
        assert!(root.join("cache/L2/memories.json").is_file());
        assert!(
            !root.join("cache/L2/memories.md").exists(),
            "L2 has no single big md file"
        );
        assert!(root.join("cache/L2/ungrouped/general.md").is_file());
        assert!(root.join("cache/L2/similar-projects.txt").is_file());
        assert!(root.join("cache/L3/memories.md").is_file());
        assert!(root.join("projects/teacher.json").is_file());
        assert!(
            !root.join("users").exists(),
            "`local` user stays at the root"
        );

        let md = fs::read_to_string(root.join("cache/L1/teacher/memories.md")).unwrap();
        assert!(md.contains("hot line"), "mirror must contain the text");

        let roundtripped = store.load(LOCAL_USER).unwrap().unwrap();
        assert_eq!(roundtripped.records.len(), 3);
        assert_eq!(roundtripped.count_in(Level::L1), 1);
        assert_eq!(roundtripped.embedder, "hashing:512");
        // links from ProjectInfo are written to the text file and survive load
        assert!(roundtripped.projects["teacher"]
            .similar
            .contains(&"music".into()));
    }

    #[test]
    fn hand_edited_links_survive_recomputation() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();

        let mut db = UserDb::new("hashing:512".into(), 512);
        db.projects.insert(
            "a".into(),
            ProjectInfo {
                project_id: "a".into(),
                name: "a".into(),
                tags: vec![],
                components: vec![],
                descriptor: "alpha".into(),
                descriptor_vector: vec![],
                similar: vec![],
                uses: vec![],
                group: None,
                created_at_ms: 1,
            },
        );
        store.save(LOCAL_USER, &db).unwrap();

        // human adds a link the engine never computed
        let link_path = dir.path().join("cache/L2/similar-projects.txt");
        let mut text = fs::read_to_string(&link_path).unwrap();
        text.push_str("a games\n");
        fs::write(&link_path, text).unwrap();

        store.save(LOCAL_USER, &db).unwrap(); // engine recompute must keep it
        let links = fs::read_to_string(&link_path).unwrap();
        assert!(links.contains("a games"), "manual pairs survive: {links}");

        let loaded = store.load(LOCAL_USER).unwrap().unwrap();
        assert!(loaded.projects["a"].similar.contains(&"games".into()));
    }

    #[test]
    fn groups_membership_file_and_mirrors() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();

        let mut db = UserDb::new("hashing:512".into(), 512);
        for (id, group) in [("a", Some("g1")), ("b", Some("g1")), ("c", None)] {
            db.projects.insert(
                id.into(),
                ProjectInfo {
                    project_id: id.into(),
                    name: id.into(),
                    tags: vec![],
                    components: vec![],
                    descriptor: id.into(),
                    descriptor_vector: vec![],
                    similar: vec![],
                    uses: vec![],
                    group: group.map(|g| g.to_string()),
                    created_at_ms: 1,
                },
            );
        }
        let mut clap_pref = record(Level::L2, Some("a"), "shared clap preference");
        clap_pref.topic = Some("tooling".into());
        db.records.push(clap_pref);
        let mut group_owned = record(Level::L2, None, "group-wide convention");
        group_owned.group = Some("g1".into());
        group_owned.topic = Some("preferences".into());
        db.records.push(group_owned);
        db.records
            .push(record(Level::L2, Some("c"), "ungrouped note"));
        store.save(LOCAL_USER, &db).unwrap();

        let root = dir.path();
        let links = fs::read_to_string(root.join("cache/L2/groups.txt")).unwrap();
        assert!(links.lines().any(|l| l == "a g1"), "{links}");
        assert!(links.lines().any(|l| l == "b g1"), "{links}");
        assert!(
            !links.lines().any(|l| l.starts_with("c ")),
            "projects without groups are not listed: {links}"
        );

        // per-topic mirrors inside the group dir
        let tooling = fs::read_to_string(root.join("cache/L2/groups/g1/tooling.md")).unwrap();
        assert!(tooling.contains("shared clap preference"));
        assert!(
            !tooling.contains("group-wide convention"),
            "topics split into separate files"
        );
        let prefs = fs::read_to_string(root.join("cache/L2/groups/g1/preferences.md")).unwrap();
        assert!(prefs.contains("group-wide convention"));
        // untopic'd, ungrouped records land in ungrouped/<default-topic>.md
        let ungrouped = fs::read_to_string(root.join("cache/L2/ungrouped/general.md")).unwrap();
        assert!(ungrouped.contains("ungrouped note"));
        assert!(
            !root.join("cache/L2/memories.md").exists(),
            "L2 is no longer one big md file"
        );
        assert!(!root.join("cache/L2/groups/g1/memories.md").exists());

        // a topic file whose records all move away is cleaned up
        fs::write(root.join("cache/L2/groups/g1/stale.md"), "# stale\n").unwrap();
        store.save(LOCAL_USER, &db).unwrap();
        assert!(!root.join("cache/L2/groups/g1/stale.md").exists());

        // hand edits in the file win on load
        let path = root.join("cache/L2/groups.txt");
        let text = fs::read_to_string(&path).unwrap().replace("b g1", "b g2");
        fs::write(&path, text).unwrap();
        let loaded = store.load(LOCAL_USER).unwrap().unwrap();
        assert_eq!(loaded.projects["b"].group.as_deref(), Some("g2"));
        assert_eq!(loaded.projects["a"].group.as_deref(), Some("g1"));
    }

    #[test]
    fn additional_users_live_under_users_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();
        let db = UserDb::new("hashing:512".into(), 512);

        store.save(LOCAL_USER, &db).unwrap();
        store.save("friend", &db).unwrap();

        assert!(dir.path().join("cache/L3").is_dir());
        assert!(dir.path().join("users/friend/cache/L3").is_dir());
        let users = store.users().unwrap();
        assert_eq!(users, vec!["friend".to_string(), LOCAL_USER.to_string()]);
    }

    #[test]
    fn current_project_marker_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();
        assert_eq!(store.current_project(LOCAL_USER).unwrap(), None);
        store.set_current_project(LOCAL_USER, "teacher").unwrap();
        assert_eq!(
            store.current_project(LOCAL_USER).unwrap(),
            Some("teacher".to_string())
        );
        assert!(store.set_current_project(LOCAL_USER, "../evil").is_err());
    }

    #[test]
    fn stale_project_files_are_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();
        let mut db = UserDb::new("hashing:512".into(), 512);
        db.projects.insert(
            "a".into(),
            ProjectInfo {
                project_id: "a".into(),
                name: "a".into(),
                tags: vec![],
                components: vec![],
                descriptor: "a".into(),
                descriptor_vector: vec![],
                similar: vec![],
                uses: vec![],
                group: None,
                created_at_ms: 1,
            },
        );
        store.save(LOCAL_USER, &db).unwrap();
        assert!(dir.path().join("projects/a.json").is_file());

        db.projects.remove("a");
        store.save(LOCAL_USER, &db).unwrap();
        assert!(!dir.path().join("projects/a.json").exists());
    }

    #[test]
    fn rejects_bad_user_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path()).unwrap();
        assert!(store.save("../evil", &UserDb::new("x".into(), 1)).is_err());
        assert!(store.save(".hidden", &UserDb::new("x".into(), 1)).is_err());
    }

    #[test]
    fn uses_file_roundtrip_directional_with_hand_edits() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayeredDirStore::new(dir.path()).unwrap();

        let mut db = UserDb::new("hashing:512".into(), 512);
        for (id, uses) in [("a", vec!["b"]), ("b", vec![])] {
            db.projects.insert(
                id.into(),
                ProjectInfo {
                    project_id: id.into(),
                    name: id.into(),
                    tags: vec![],
                    components: vec![],
                    descriptor: id.into(),
                    descriptor_vector: vec![],
                    similar: vec![],
                    uses: uses.into_iter().map(String::from).collect(),
                    group: None,
                    created_at_ms: 1,
                },
            );
        }
        store.save(LOCAL_USER, &db).unwrap();

        // directional pairs, one per line — `b a` is NOT the same link
        let path = dir.path().join("cache/uses.txt");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.lines().any(|l| l == "a b"), "{text}");
        assert!(
            !text.lines().any(|l| l == "b a"),
            "direction matters: {text}"
        );

        // hand-added lines attach to their registered using-project on the
        // next load (the save below uses that loaded db — like every engine
        // flow, which loads before it ever writes)
        let path2 = dir.path().join("cache/uses.txt");
        let mut text = fs::read_to_string(&path2).unwrap();
        text.push_str("b a\nc d\n");
        fs::write(&path2, text).unwrap();
        let loaded = store.load(LOCAL_USER).unwrap().unwrap();
        assert_eq!(loaded.projects["a"].uses, vec!["b".to_string()]);
        assert_eq!(
            loaded.projects["b"].uses,
            vec!["a".to_string()],
            "hand-added lines attach on load"
        );
        assert!(!loaded.projects.contains_key("c"));

        // the loaded db persists everything; the unknown pair survives too
        store.save(LOCAL_USER, &loaded).unwrap();
        let text = fs::read_to_string(&path2).unwrap();
        assert!(text.lines().any(|l| l == "b a"), "{text}");
        assert!(text.lines().any(|l| l == "c d"), "hand edits survive: {text}");

        // a project registered with the link keeps it across save + load
        let mut db = loaded;
        db.projects.insert(
            "c".into(),
            ProjectInfo {
                project_id: "c".into(),
                name: "c".into(),
                tags: vec![],
                components: vec![],
                descriptor: "c".into(),
                descriptor_vector: vec![],
                similar: vec![],
                uses: vec!["d".into()],
                group: None,
                created_at_ms: 1,
            },
        );
        store.save(LOCAL_USER, &db).unwrap();
        let loaded = store.load(LOCAL_USER).unwrap().unwrap();
        assert_eq!(loaded.projects["c"].uses, vec!["d".to_string()]);
    }

    #[test]
    fn json_store_roundtrip_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonFileStore::new(dir.path()).unwrap();
        assert!(store.load("u1").unwrap().is_none());

        let db = UserDb::new("hashing:512".into(), 512);
        store.save("u1", &db).unwrap();
        let loaded = store.load("u1").unwrap().unwrap();
        assert_eq!(loaded.embedder, "hashing:512");

        store.save("u2", &db).unwrap();
        let mut users = store.users().unwrap();
        users.sort();
        assert_eq!(users, vec!["u1".to_string(), "u2".to_string()]);
    }
}
