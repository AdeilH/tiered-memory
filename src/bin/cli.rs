//! `tiered-memory` — the standalone binary: HTTP service + local CLI.
//!
//! ```text
//! tiered-memory serve                   start the HTTP service (default)
//! tiered-memory init                    register THIS project (run in the project dir)
//! tiered-memory projects                list projects using tiered memory
//! tiered-memory select                  interactively pick the current project
//! tiered-memory params                  show adjusted parameters (uses selection)
//! tiered-memory remember "text"         store a memory (shell-out friendly)
//! tiered-memory recall "query"          layered search (shell-out friendly)
//! tiered-memory stats                   per-layer counts vs capacity
//! ```
//!
//! Data lives under `$TM_DATA_DIR` (default `~/tiered-memory`), laid out as
//! `cache/L1/<project>/`, `cache/L2/`, `cache/L3/` — see README. The default
//! user is `local`.
//!
//! Any project — Rust or not — talks to the installed binary either over HTTP
//! (`tiered-memory serve` + any HTTP client) or by shelling out to
//! `remember`/`recall`. Write commands go through the running service when it
//! is reachable (HTTP-first) so a live service and CLI writes never go stale.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
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
        Some("init") => init(),
        Some("projects") => projects(),
        Some("select") => select(),
        Some("params") => params(),
        Some("remember") => remember(),
        Some("recall") => recall(),
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
  tiered-memory serve                    start the HTTP service on {DEFAULT_BIND}
  tiered-memory init [--name N] [--id ID] [--descriptor T] [--user U]
                                         register THIS directory as a project
                                         (writes ./tiered-memory.json)
  tiered-memory projects [--user U]      list projects using tiered memory
  tiered-memory select  [--user U] [--project P]
                                         pick the current project (interactive without P)
  tiered-memory params  [--user U] [--project P]
                                         show the adjusted parameter set
  tiered-memory remember \"text\" [--project P] [--kind KIND]
                        [--param k=v]... [--pin] [--ttl DAYS] [--user U]
                                         store a memory
  tiered-memory recall \"query\" [--project P] [--k N] [--min F] [--user U]
                                         layered search
  tiered-memory stats   [--user U]       per-layer counts vs capacity

Non-Rust projects use the installed binary two ways: HTTP (serve + any client)
or by shelling out to `remember` / `recall` / `params`.

ENV:
  TM_DATA_DIR    data root (default: ~/tiered-memory)
  TM_STORE       `layered` (default, cache/L1|L2|L3 layout) or `flat` (JSON per user)
  TM_USER        default user (default: local)
  TM_EMBEDDER    hashing | local | http   (see README for per-backend vars)
  TM_BIND        serve bind address (default: {DEFAULT_BIND})
  TM_BASE_URL    service URL the CLI talks to (default: http://{DEFAULT_BIND})"
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

fn local_engine() -> Result<MemoryEngine, String> {
    let embedder = EmbedderConfig::from_env()
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    Ok(MemoryEngine::new(
        store()?,
        embedder,
        EngineConfig::from_env(),
    ))
}

/// Value of `--flag <value>` from the arg list.
fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Boolean switch `--flag`.
fn arg_switch(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// First argument that is not a flag or a flag value.
fn positional(args: &[String]) -> Option<String> {
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with('-') {
            // flags that consume a value
            if matches!(
                a.as_str(),
                "--user" | "--project" | "--name" | "--id" | "--descriptor" | "--kind"
                    | "--param" | "--ttl" | "--k" | "--min"
            ) {
                skip_next = true;
            }
            continue;
        }
        return Some(a.clone());
    }
    None
}

// -- HTTP-first helper --------------------------------------------------------

enum Api {
    Ok(serde_json::Value),
    Err(String),
    Unreachable,
}

fn api_base() -> String {
    std::env::var("TM_BASE_URL").unwrap_or_else(|_| format!("http://{DEFAULT_BIND}"))
}

/// POST to the running service, if there is one. `Err` means the service IS
/// reachable but rejected the call (surface it — don't silently fall back);
/// `Unreachable` lets the caller use the local store directly.
fn api_post(path: &str, body: &serde_json::Value) -> Api {
    let mut req = ureq::post(&format!("{}{}", api_base(), path))
        .timeout(std::time::Duration::from_millis(2000));
    if let Ok(token) = std::fs::read_to_string(data_root().join("token")) {
        let t = token.trim();
        if !t.is_empty() {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
    }
    match req.send_json(body) {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(v) => Api::Ok(v),
            Err(e) => Api::Err(format!("unreadable response from service: {e}")),
        },
        Err(ureq::Error::Status(code, resp)) => {
            let msg = resp.into_string().unwrap_or_default();
            Api::Err(format!("service returned {code}: {msg}"))
        }
        Err(_) => Api::Unreachable,
    }
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
    eprintln!("endpoints: POST /v1/remember /v1/recall /v1/params /v1/feedback /v1/projects /v1/consolidate /v1/forget /v1/reindex · GET /v1/health /v1/stats/{{user}} /v1/projects/{{user}} · CLI: init, projects, select, params, remember, recall, stats");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("tiered-memory: shutting down");
        })
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// -- init --------------------------------------------------------------------

/// Register the current directory as a project. Detects the project name and
/// description from `package.json`, `Cargo.toml`, `pyproject.toml` or the
/// directory name, registers it in the memory store (through the running
/// service when one is up), and writes a `tiered-memory.json` marker so hosts
/// and agents can discover how to reach this project's memory.
fn init() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;

    let (detected_name, detected_desc) = detect_project(&cwd);
    let name = arg_value(&args, "--name").unwrap_or(detected_name.clone());
    let descriptor = {
        let given = arg_value(&args, "--descriptor");
        given
            .or_else(|| {
                if detected_desc.is_empty() {
                    None
                } else {
                    Some(detected_desc.clone())
                }
            })
            .unwrap_or_else(|| name.clone())
    };
    let project_id = match arg_value(&args, "--id") {
        Some(id) => id,
        None => slugify(&name),
    };
    if project_id.is_empty() || project_id.starts_with('.') {
        return Err(format!(
            "cannot derive a project id from `{name}` — pass --id explicitly"
        ));
    }

    let body = serde_json::json!({
        "user": user,
        "project_id": project_id,
        "name": name,
        "descriptor": descriptor,
    });
    let via = match api_post("/v1/projects", &body) {
        Api::Ok(_) => "running service".to_string(),
        Api::Err(e) => return Err(e),
        Api::Unreachable => {
            local_engine()?
                .register_project(tiered_memory::ProjectInput {
                    user: user.clone(),
                    project_id: project_id.clone(),
                    name: Some(name.clone()),
                    tags: vec![],
                    components: vec![],
                    descriptor: Some(descriptor.clone()),
                })
                .map_err(|e| e.to_string())?;
            "local store (no service running)".to_string()
        }
    };

    let config = serde_json::json!({
        "version": 1,
        "project_id": project_id,
        "user": user,
        "service": api_base(),
    });
    let config_text =
        serde_json::to_string_pretty(&config).map_err(|e| format!("serialize config: {e}"))?;
    let config_path = cwd.join("tiered-memory.json");
    std::fs::write(&config_path, format!("{config_text}\n"))
        .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;

    println!("registered `{project_id}` via {via}");
    if project_id != detected_name {
        println!("  ({detected_name} → {project_id})");
    }
    println!("wrote {}", config_path.display());
    println!("\nnext:");
    println!("  tiered-memory remember \"prefers analogies from games\" --project {project_id}");
    println!("  tiered-memory recall \"how should I explain recursion?\" --project {project_id}");
    println!("  tiered-memory params --project {project_id}");
    println!("anywhere in this project: tiered-memory select --project {project_id}");
    Ok(())
}

/// (name, description) for the project at `dir`, best-effort.
fn detect_project(dir: &Path) -> (String, String) {
    // package.json
    if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            if !name.is_empty() {
                let desc = v
                    .get("description")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                return (name, desc);
            }
        }
    }
    // Cargo.toml ([package] name) / pyproject.toml ([project] name)
    for (file, section) in [("Cargo.toml", "[package]"), ("pyproject.toml", "[project]")] {
        if let Ok(text) = std::fs::read_to_string(dir.join(file)) {
            if let Some(name) = toml_field(&text, section, "name") {
                let desc = toml_field(&text, section, "description").unwrap_or_default();
                return (name, desc);
            }
        }
    }
    // fallback: directory name
    let name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    (name, String::new())
}

fn toml_field(text: &str, section: &str, field: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.starts_with(section);
            continue;
        }
        if !in_section {
            continue;
        }
        let mut parts = line.splitn(2, '=');
        let key = parts.next()?.trim();
        if key == field {
            let value = parts.next()?.trim().trim_matches('"').to_string();
            return Some(value);
        }
    }
    None
}

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
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
        println!("no projects registered for `{user}` yet — cd into a project and run `tiered-memory init`");
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

/// Explicit `--project`, else the `select` marker.
fn resolve_project(args: &[String], user: &str) -> Result<String, String> {
    if let Some(p) = arg_value(args, "--project") {
        return Ok(p);
    }
    // the project this directory was init'ed as, else the global marker
    if let Ok(text) = std::fs::read_to_string("tiered-memory.json") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if v.get("user").and_then(|x| x.as_str()) == Some(user) {
                if let Some(id) = v.get("project_id").and_then(|x| x.as_str()) {
                    return Ok(id.to_string());
                }
            }
        }
    }
    current_selection(user)
        .ok_or("no --project given, no tiered-memory.json here, and no selection — run `tiered-memory init` or `select`".into())
}

// -- select ------------------------------------------------------------------

fn flags(args: &[String]) -> (String, Option<String>) {
    (arg_value(args, "--user").unwrap_or_else(default_user), arg_value(args, "--project"))
}

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

// -- remember ----------------------------------------------------------------

fn parse_param(s: &str) -> Result<(String, tiered_memory::ParamValue), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("--param expects key=value, got `{s}`"))?;
    let value = if let Ok(n) = v.parse::<f64>() {
        tiered_memory::ParamValue::Number(n)
    } else if v == "true" || v == "false" {
        tiered_memory::ParamValue::Bool(v == "true")
    } else {
        tiered_memory::ParamValue::Text(v.to_string())
    };
    Ok((k.to_string(), value))
}

fn remember() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let text = positional(&args).ok_or("give the memory as a quoted argument")?;
    let project = if arg_switch(&args, "--global") {
        None
    } else {
        Some(resolve_project(&args, &user)?)
    };

    let mut params = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--param" {
            if let Some(v) = args.get(i + 1) {
                let (k, val) = parse_param(v).map_err(|e| e.to_string())?;
                params.insert(k, val);
            }
            i += 1;
        }
        i += 1;
    }

    let body = serde_json::json!({
        "user": user,
        "text": text,
        "project_id": project.clone(),
        "params": if params.is_empty() { None } else { Some(params.clone()) },
        "pinned": arg_switch(&args, "--pin"),
        "ttl_days": arg_value(&args, "--ttl").and_then(|t| t.parse::<f64>().ok()),
    });
    match api_post("/v1/remember", &body) {
        Api::Ok(out) => {
            println!(
                "stored `{}` (deduped: {})",
                out["id"].as_str().unwrap_or("?"),
                out["deduped"].as_bool().unwrap_or(false)
            );
        }
        Api::Err(e) => return Err(e),
        Api::Unreachable => {
            let out = local_engine()?
                .remember(tiered_memory::RememberInput {
                    user: user.clone(),
                    text: text.clone(),
                    project_id: project,
                    kind: None,
                    params: Some(params),
                    key_hint: None,
                    confidence: None,
                    pinned: Some(arg_switch(&args, "--pin")),
                    ttl_days: arg_value(&args, "--ttl").and_then(|t| t.parse::<f64>().ok()),
                    level: None,
                })
                .map_err(|e| e.to_string())?;
            println!("stored `{}` (local store, no service running)", out.id);
        }
    }
    Ok(())
}

// -- recall ------------------------------------------------------------------

fn recall() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let query = positional(&args).ok_or("give the query as a quoted argument")?;
    let project = resolve_project(&args, &user).ok();
    let k = arg_value(&args, "--k").and_then(|v| v.parse::<usize>().ok());
    let min = arg_value(&args, "--min").and_then(|v| v.parse::<f32>().ok());

    let body = serde_json::json!({
        "user": user,
        "query": query,
        "project_id": project,
        "k": k,
        "min_similarity": min,
    });
    let out = match api_post("/v1/recall", &body) {
        Api::Ok(v) => v,
        Api::Err(e) => return Err(e),
        Api::Unreachable => {
            let out = local_engine()?
                .recall(tiered_memory::RecallInput {
                    user: user.clone(),
                    query: query.clone(),
                    project_id: project.clone(),
                    k,
                    min_similarity: min,
                    write_allocate: None,
                })
                .map_err(|e| e.to_string())?;
            serde_json::to_value(&out).map_err(|e| e.to_string())?
        }
    };

    let hits = out["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        println!("(no memories matched `{query}`)");
        return Ok(());
    }
    let scope = project
        .map(|p| format!("project `{p}`"))
        .unwrap_or_else(|| "global".to_string());
    println!("recalled for `{query}` ({scope}):");
    for h in &hits {
        println!(
            "  [{:?}] sim={:.2}  {}",
            h["level"].as_str().unwrap_or("?"),
            h["similarity"].as_f64().unwrap_or(0.0),
            h["text"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

// -- params ------------------------------------------------------------------

fn params() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let (user, explicit) = flags(&args);
    let project = match explicit {
        Some(p) => p,
        None => resolve_project(&args, &user)?,
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
