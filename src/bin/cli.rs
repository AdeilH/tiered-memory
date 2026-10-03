//! `tiered-memory` — the standalone binary: HTTP service + local CLI.
//!
//! ```text
//! tiered-memory serve                   start the HTTP service (default)
//! tiered-memory projects                list projects using tiered memory
//! tiered-memory select                  interactively pick the current project
//! tiered-memory params                  show adjusted parameters (uses selection)
//! tiered-memory stats                   per-layer counts vs capacity
//! ```
//!
//! Data lives under `$TM_DATA_DIR` (default `~/tiered-memory`), laid out as
//! `cache/L1/<project>/`, `cache/L2/`, `cache/L3/` — see README. The default
//! user is `local`.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tiered_memory::{
    default_data_dir, EmbedderConfig, EngineConfig, JsonFileStore, LayeredDirStore, MemoryEngine,
    MemoryStore, ServerState, DEFAULT_BIND, LOCAL_USER,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") => serve().await,
        Some("projects") => projects(),
        Some("select") => select(),
        Some("params") => params(),
        Some("stats") => stats(),
        Some("help") | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}` — run `tiered-memory help`")),
    };
    if let Err(err) = result {
        eprintln!("tiered-memory: {err}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!(
        "tiered-memory — layered learner memory (L1/L2/L3) as a standalone binary

USAGE:
  tiered-memory serve                 start the HTTP service on {DEFAULT_BIND}
  tiered-memory projects [--user U]   list projects using tiered memory
  tiered-memory select  [--user U] [--project P]
                                      pick the current project (interactive without P)
  tiered-memory params  [--user U] [--project P]
                                      show the adjusted parameter set
  tiered-memory stats   [--user U]    per-layer counts vs capacity

ENV:
  TM_DATA_DIR    data root (default: ~/tiered-memory)
  TM_STORE       `layered` (default, cache/L1|L2|L3 layout) or `flat` (JSON per user)
  TM_USER        default user (default: local)
  TM_EMBEDDER    hashing | local | http   (see README for per-backend vars)
  TM_BIND        serve bind address (default: {DEFAULT_BIND})"
    );
}

fn data_root() -> PathBuf {
    std::env::var("TM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_dir())
}

fn default_user() -> String {
    std::env::var("TM_USER").unwrap_or_else(|_| LOCAL_USER.to_string())
}

fn store() -> Result<Arc<dyn MemoryStore>, String> {
    let root = data_root();
    match std::env::var("TM_STORE").as_deref() {
        Ok("flat") => Ok(Arc::new(
            JsonFileStore::new(&root).map_err(|e| e.to_string())?,
        )),
        _ => Ok(Arc::new(
            LayeredDirStore::new(&root).map_err(|e| e.to_string())?,
        )),
    }
}

/// Read `--user` / `--project` flags from the arg list.
fn flags(args: &[String]) -> (String, Option<String>) {
    let mut user = default_user();
    let mut project = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--user" if i + 1 < args.len() => {
                user = args[i + 1].clone();
                i += 1;
            }
            "--project" if i + 1 < args.len() => {
                project = Some(args[i + 1].clone());
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    (user, project)
}

// -- serve -------------------------------------------------------------------

async fn serve() -> Result<(), String> {
    let root = data_root();
    let bind = std::env::var("TM_BIND").unwrap_or_else(|_| DEFAULT_BIND.into());

    let store = store()?;
    let embedder = EmbedderConfig::from_env()
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let engine = Arc::new(MemoryEngine::new(store, embedder, EngineConfig::from_env()));

    // Optional bearer token: if `{data}/token` exists, its trimmed contents
    // become the required secret (all routes except /v1/health).
    let token = std::fs::read_to_string(root.join("token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let auth = if token.is_some() {
        "token auth ON"
    } else {
        "no token (loopback only)"
    };

    let health = engine.health();
    let app = tiered_memory::build_router(Arc::new(ServerState {
        engine: engine.clone(),
        token,
    }));

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| format!("cannot bind {bind}: {e}"))?;
    eprintln!(
        "tiered-memory v{} | embedder {} ({} dims) | data: {} | {}",
        health.version, health.embedder, health.dims, root.display(), auth
    );
    eprintln!("endpoints: POST /v1/remember /v1/recall /v1/params /v1/feedback /v1/projects /v1/consolidate /v1/forget /v1/reindex · GET /v1/health /v1/stats/{{user}} /v1/projects/{{user}} · CLI: projects, select, params, stats");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("tiered-memory: shutting down");
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// -- projects ----------------------------------------------------------------

fn projects() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (user, _) = flags(&args);
    let store = store()?;
    let db = store
        .load(&user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}`"))?;

    if db.projects.is_empty() {
        println!("no projects registered for `{user}` yet — POST /v1/projects or `tiered-memory select`");
        return Ok(());
    }
    let current = current_selection(&user);
    println!("projects using tiered memory for `{user}`:\n");
    for (id, p) in &db.projects {
        let l1 = db
            .records
            .iter()
            .filter(|r| r.level == tiered_memory::Level::L1 && r.project_id.as_deref() == Some(id))
            .count();
        let cur = if current.as_deref() == Some(id.as_str()) {
            "  ← current"
        } else {
            ""
        };
        let name = if p.name.is_empty() { id.as_str() } else { p.name.as_str() };
        println!("  {id:<24} L1 {l1:>3}  similar: {:?}  {name}{cur}", p.similar);
    }
    println!("\nselect one with: tiered-memory select --user {user} [--project <id>]");
    Ok(())
}

fn current_selection(user: &str) -> Option<String> {
    LayeredDirStore::new(data_root())
        .ok()?
        .current_project(user)
        .ok()
        .flatten()
}

// -- select ------------------------------------------------------------------

fn select() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (user, explicit) = flags(&args);
    let store = store()?;

    if let Some(id) = explicit {
        LayeredDirStore::new(data_root())
            .map_err(|e| e.to_string())?
            .set_current_project(&user, &id)
            .map_err(|e| e.to_string())?;
        println!("current project for `{user}` → {id}");
        return Ok(());
    }

    let db = store
        .load(&user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}` — register projects first"))?;
    let ids: Vec<&String> = db.projects.keys().collect();
    if ids.is_empty() {
        return Err(format!("no projects registered for `{user}` yet"));
    }

    let current = current_selection(&user);
    println!("select the current project for `{user}`:");
    for (i, id) in ids.iter().enumerate() {
        let name = &db.projects[*id].name;
        let cur = if current.as_deref() == Some(id.as_str()) {
            "  ← current"
        } else {
            ""
        };
        println!("  [{}] {id:<24} {name}{cur}", i + 1);
    }
    print!("\nnumber (1-{}): ", ids.len());
    std::io::stdout().flush().map_err(|e| e.to_string())?;

    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let n: usize = line
        .trim()
        .parse()
        .map_err(|_| format!("not a number: {}", line.trim()))?;
    if n == 0 || n > ids.len() {
        return Err(format!("out of range: {n}"));
    }
    let chosen = ids[n - 1].clone();
    LayeredDirStore::new(data_root())
        .map_err(|e| e.to_string())?
        .set_current_project(&user, &chosen)
        .map_err(|e| e.to_string())?;
    println!("current project for `{user}` → {chosen}");
    Ok(())
}

// -- params ------------------------------------------------------------------

fn params() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (user, explicit) = flags(&args);
    let project = match explicit {
        Some(p) => p,
        None => current_selection(&user)
            .ok_or("no --project given and no selection — run `tiered-memory select`")?,
    };

    let store = store()?;
    let embedder = EmbedderConfig::from_env()
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let engine = MemoryEngine::new(store, embedder, EngineConfig::from_env());

    let suggestions = engine
        .adjusted_parameters(&user, Some(&project))
        .map_err(|e| e.to_string())?;
    println!("adjusted parameters for `{user}` · `{project}`:");
    if suggestions.is_empty() {
        println!("  (nothing learned yet)");
    }
    for s in &suggestions {
        println!(
            "  {:<24} = {:<12} [{:?}] confidence {:.2}  alternatives: {}",
            s.key,
            s.value.as_text(),
            s.source,
            s.confidence,
            s.alternatives
                .iter()
                .map(|a| format!("{} ({:?})", a.value.as_text(), a.source))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

// -- stats -------------------------------------------------------------------

fn stats() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (user, _) = flags(&args);
    let store = store()?;
    let db = store
        .load(&user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}`"))?;
    let cfg = EngineConfig::default();
    println!("memory stats for `{user}` ({}):", db.embedder);
    for (lvl, count, cap) in [
        ("L1", db.count_in(tiered_memory::Level::L1), cfg.l1_capacity),
        ("L2", db.count_in(tiered_memory::Level::L2), cfg.l2_capacity),
        ("L3", db.count_in(tiered_memory::Level::L3), cfg.l3_capacity),
    ] {
        println!("  {lvl}: {count:>5} / {cap}");
    }
    println!("  projects: {}", db.projects.len());
    Ok(())
}
