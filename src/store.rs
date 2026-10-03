//! Persistence backends behind the [`MemoryStore`] trait:
//!
//! * [`LayeredDirStore`] — the default. Mirrors the cache hierarchy on disk:
//!   `cache/L1/<project>/`, `cache/L2/` (one big MD + `similar-projects.txt`),
//!   `cache/L3/`. JSON files are the machine-authoritative record; every layer
//!   also gets a human-readable `memories.md` mirror regenerated on each write.
//! * [`JsonFileStore`] — one JSON file per user; simple flat alternative.

use crate::error::{MemoryError, Result};
use crate::types::{Level, MemoryKind, MemoryRecord, ProjectInfo, UserDb};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// Path-safe identifier (used for user and project directory/file names).
fn valid_path_segment(seg: &str) -> bool {
    !seg.is_empty()
        && !seg.starts_with('.')
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
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
///     L1/<project-id>/memories.json    hot, project-scoped records (machine)
///     L1/<project-id>/memories.md      …human-readable mirror
///     L2/memories.json|md              related-scope records (one big file)
///     L2/similar-projects.txt          project similarity links (hand-editable)
///     L3/memories.json|md              user-level traits common to all projects
///   users/<other-user>/…                additional users (server mode)
/// ```
///
/// The JSON files are authoritative (ids, vectors, timestamps are not
/// human-editable); the `.md` files are regenerated mirrors — read, grep and
/// diff them, but edit through the API/CLI. `similar-projects.txt` is the
/// exception: it *is* authoritative for links and hand-added pairs survive
/// automatic recomputation.
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
            Err(e) => Err(MemoryError::Storage(format!("read {}: {e}", path.display()))),
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
        atomic_write(&dir.join("memories.json"), &serde_json::to_vec_pretty(records)?)?;
        atomic_write(&dir.join("memories.md"), render_md(title, embedder, records).as_bytes())?;
        Ok(())
    }
}

impl MemoryStore for LayeredDirStore {
    fn load(&self, user: &str) -> Result<Option<UserDb>> {
        let meta_path = self.meta_path(user)?;
        let Ok(bytes) = fs::read(&meta_path) else {
            return Ok(None);
        };
        let meta: Meta = serde_json::from_slice(&bytes)
            .map_err(|e| MemoryError::Storage(format!("corrupt {}: {e}", meta_path.display())))?;
        let user_dir = self.user_dir(user)?;

        let mut db = UserDb::new(meta.embedder, meta.dims);

        // project registry
        let projects_dir = user_dir.join("projects");
        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy().to_string();
                if !name.ends_with(".json") {
                    continue;
                }
                match fs::read(entry.path()) {
                    Ok(bytes) => match serde_json::from_slice::<ProjectInfo>(&bytes) {
                        Ok(p) => {
                            db.projects.insert(p.project_id.clone(), p);
                        }
                        Err(e) => {
                            return Err(MemoryError::Storage(format!(
                                "corrupt project file {}: {e}",
                                entry.path().display()
                            )))
                        }
                    },
                    Err(e) => {
                        return Err(MemoryError::Storage(format!(
                            "read {}: {e}",
                            entry.path().display()
                        )))
                    }
                }
            }
        }

        // L1 — one folder per project
        let l1_root = Self::layer_dir(&user_dir, Level::L1);
        if let Ok(entries) = fs::read_dir(&l1_root) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let project_id = entry.file_name().to_string_lossy().to_string();
                for r in Self::read_records(&entry.path().join("memories.json"))? {
                    db.records.push(r);
                }
                // touch the registry entry so unregistered-but-used projects appear
                db.projects
                    .entry(project_id.clone())
                    .or_insert_with(|| skeleton_project(&project_id));
            }
        }
        for r in Self::read_records(&Self::layer_dir(&user_dir, Level::L2).join("memories.json"))? {
            db.records.push(r);
        }
        for r in Self::read_records(&Self::layer_dir(&user_dir, Level::L3).join("memories.json"))? {
            db.records.push(r);
        }

        // similarity links: the text file is authoritative and merges over JSON.
        // Pairs referencing a not-yet-registered project stay attached to the
        // known side and activate once that project registers.
        let links = read_link_file(&Self::layer_dir(&user_dir, Level::L2).join("similar-projects.txt"))?;
        for (a, b) in links {
            let has_a = db.projects.contains_key(&a);
            let has_b = db.projects.contains_key(&b);
            match (has_a, has_b) {
                (true, true) => {
                    if let Some(pa) = db.projects.get_mut(&a) {
                        if !pa.similar.contains(&b) {
                            pa.similar.push(b.clone());
                        }
                    }
                    if let Some(pb) = db.projects.get_mut(&b) {
                        if !pb.similar.contains(&a) {
                            pb.similar.push(a);
                        }
                    }
                }
                (true, false) => {
                    if let Some(pa) = db.projects.get_mut(&a) {
                        if !pa.similar.contains(&b) {
                            pa.similar.push(b);
                        }
                    }
                }
                (false, true) => {
                    if let Some(pb) = db.projects.get_mut(&b) {
                        if !pb.similar.contains(&a) {
                            pb.similar.push(a);
                        }
                    }
                }
                (false, false) => {} // no side to hold it; both ids unknown
            }
        }

        Ok(Some(db))
    }

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

        // project registry: write current, drop stale files
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

        // L1: one folder per registered/used project
        let mut l1_by_project: BTreeMap<String, Vec<&MemoryRecord>> = BTreeMap::new();
        for r in db.records.iter().filter(|r| r.level == Level::L1) {
            if let Some(pid) = &r.project_id {
                l1_by_project.entry(pid.clone()).or_default().push(r);
            }
        }
        let l1_root = Self::layer_dir(&user_dir, Level::L1);
        let mut keep_dirs: Vec<String> = l1_by_project.keys().cloned().collect();
        for id in db.projects.keys() {
            if !keep_dirs.contains(id) {
                keep_dirs.push(id.clone());
            }
        }
        fs::create_dir_all(&l1_root)?;
        for id in &keep_dirs {
            let empty: Vec<&MemoryRecord> = Vec::new();
            let records: &[&MemoryRecord] = match l1_by_project.get(id) {
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

        // L2 and L3
        let l2: Vec<&MemoryRecord> = db.records.iter().filter(|r| r.level == Level::L2).collect();
        Self::write_records(
            &Self::layer_dir(&user_dir, Level::L2),
            "L2 · related projects & components",
            &db.embedder,
            &l2,
        )?;

        let l3: Vec<&MemoryRecord> = db.records.iter().filter(|r| r.level == Level::L3).collect();
        Self::write_records(
            &Self::layer_dir(&user_dir, Level::L3),
            "L3 · learner traits (all projects)",
            &db.embedder,
            &l3,
        )?;

        // similarity links file: existing hand-edits ∪ computed links
        let link_path = Self::layer_dir(&user_dir, Level::L2).join("similar-projects.txt");
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
        created_at_ms: crate::engine::system_now_ms(),
    }
}

fn read_link_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MemoryError::Storage(format!("read {}: {e}", path.display()))),
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
                created_at_ms: 1,
            },
        );
        db.records.push(record(Level::L1, Some("teacher"), "hot line"));
        db.records.push(record(Level::L2, Some("teacher"), "warm line"));
        db.records.push(record(Level::L3, None, "global trait"));
        store.save(LOCAL_USER, &db).unwrap();

        // layout: L1 per project, L2 big file + links, L3 user-level
        let root = dir.path();
        assert!(root.join("cache/L1/teacher/memories.json").is_file());
        assert!(root.join("cache/L1/teacher/memories.md").is_file());
        assert!(root.join("cache/L2/memories.json").is_file());
        assert!(root.join("cache/L2/memories.md").is_file());
        assert!(root.join("cache/L2/similar-projects.txt").is_file());
        assert!(root.join("cache/L3/memories.md").is_file());
        assert!(root.join("projects/teacher.json").is_file());
        assert!(!root.join("users").exists(), "`local` user stays at the root");

        let md = fs::read_to_string(root.join("cache/L1/teacher/memories.md")).unwrap();
        assert!(md.contains("hot line"), "mirror must contain the text");

        let roundtripped = store.load(LOCAL_USER).unwrap().unwrap();
        assert_eq!(roundtripped.records.len(), 3);
        assert_eq!(roundtripped.count_in(Level::L1), 1);
        assert_eq!(roundtripped.embedder, "hashing:512");
        // links from ProjectInfo are written to the text file and survive load
        assert!(roundtripped.projects["teacher"].similar.contains(&"music".into()));
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
