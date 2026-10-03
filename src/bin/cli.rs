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

use std::io::{BufRead, Read, Write};
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
        Some("sync") => sync_cmd(),
        Some("credentials") => credentials(),
        Some("auth") => auth(),
        Some("models") => models(),
        Some("install-skill") => install_skill(),
        Some("env") => print_env(),
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
  tiered-memory sync [--file F | --text T | --stdin] [--project P] [--user U]
                     [--dry-run]
                                         gather all 3 layers, extract new/changed
                                         knowledge with the configured LLM, write it
                                         back into the right layers
  tiered-memory credentials              interactive setup: pick a provider, enter
                                         the key, then search the provider's live
                                         model list (flags --base-url/--api-key/
                                         --model skip the TUI for scripts)
  tiered-memory credentials show | clear
  tiered-memory auth on | off | show     bearer-token auth for the HTTP service
  tiered-memory models                   list the configured provider's models
  tiered-memory install-skill [--dir D]   install the /tiered-memory agent skill
                                         (default dir: ~/.agents/skills)
  tiered-memory env                      print exports for `eval \"$(tiered-memory env)\"`
                                         (PATH + TM_DATA_DIR; a process cannot
                                         export into its parent shell itself)
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

    // Safety rail (docs/SECURITY_ANALYSIS.md M5): a non-loopback bind turns
    // the service into a network API — require a token, or an explicit opt-out.
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(&bind);
    let loopback = matches!(host, "" | "127.0.0.1" | "localhost" | "::1" | "[::1]");
    if !loopback && token.is_none() && std::env::var("TM_ALLOW_INSECURE").as_deref() != Ok("1") {
        return Err(format!(
            "refusing to bind non-loopback address `{bind}` without auth — write a secret to {} (Bearer token) or set TM_ALLOW_INSECURE=1 to override",
            root.join("token").display()
        ));
    }

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

// -- sync --------------------------------------------------------------------

/// The `/tiered-memory` update pass: gather all three layers, extract new or
/// changed knowledge from the given conversation with the configured
/// OpenAI-compatible LLM, and write it back into the right layers. Applies
/// through the running service when one is up (same policy as `remember`).
fn sync_cmd() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let project = resolve_project(&args, &user)?;

    let conversation = if arg_switch(&args, "--stdin") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        buf
    } else if let Some(path) = arg_value(&args, "--file") {
        std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?
    } else {
        positional(&args).ok_or(
            "give the conversation via --file <path>, --stdin, or as a quoted argument",
        )?
    };

    let config = tiered_memory::LlmConfig::resolve(None, &data_root())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| {
            "no LLM credentials — run `tiered-memory credentials` (interactive setup)".to_string()
        })?;
    let llm = tiered_memory::LlmClient::new(config).map_err(|e| e.to_string())?;

    let input = tiered_memory::SyncInput {
        user: user.clone(),
        project_id: project.clone(),
        conversation,
    };
    let engine = local_engine()?;
    let plan = tiered_memory::plan(&engine, &llm, &input).map_err(|e| e.to_string())?;

    if plan.entries.is_empty() {
        println!("sync: nothing new to store — all layers already cover this conversation");
        return Ok(());
    }
    if arg_switch(&args, "--dry-run") {
        println!("sync plan (dry run) for `{user}` · `{project}`:");
        for e in &plan.entries {
            println!("  [{:?}] {}{}", e.level, e.text, fmt_params(&e.params));
        }
        return Ok(());
    }

    // HTTP-first: route each entry through the running service when reachable.
    let mut report = tiered_memory::SyncReport {
        raw_model_reply: plan.raw_model_reply.clone(),
        ..Default::default()
    };
    for entry in &plan.entries {
        let body = serde_json::json!({
            "user": user,
            "text": entry.text,
            "project_id": if entry.level == tiered_memory::Level::L3 { None } else { Some(&project) },
            "level": entry.level,
            "params": if entry.params.is_empty() { None } else { Some(entry.params.clone()) },
            "key_hint": entry.key,
            "confidence": entry.confidence,
        });
        match api_post("/v1/remember", &body) {
            Api::Ok(_) => report.stored.push(entry.clone()),
            Api::Err(e) => report.skipped.push(e),
            Api::Unreachable => {
                // fall back to direct writes on the SAME engine so its cache
                // stays coherent for the params_after read below
                match engine.remember(tiered_memory::RememberInput {
                    user: user.clone(),
                    text: entry.text.clone(),
                    project_id: if entry.level == tiered_memory::Level::L3 {
                        None
                    } else {
                        Some(project.clone())
                    },
                    kind: None,
                    params: if entry.params.is_empty() {
                        None
                    } else {
                        Some(entry.params.clone())
                    },
                    key_hint: entry.key.clone(),
                    confidence: Some(entry.confidence),
                    pinned: None,
                    ttl_days: None,
                    level: Some(entry.level),
                }) {
                    Ok(_) => report.stored.push(entry.clone()),
                    Err(e) => report.skipped.push(format!("{} ({e})", entry.text)),
                }
            }
        }
    }
    for s in engine
        .adjusted_parameters(&user, Some(&project))
        .map_err(|e| e.to_string())?
    {
        report.params_after.insert(s.key, s.value);
    }

    println!(
        "sync for `{user}` · `{project}`: {} stored, {} skipped",
        report.stored.len(),
        report.skipped.len()
    );
    for e in &report.stored {
        println!("  [{:?}] {}{}", e.level, e.text, fmt_params(&e.params));
    }
    for s in &report.skipped {
        println!("  (skipped) {s}");
    }
    if !report.params_after.is_empty() {
        println!("\nadjusted parameters now:");
        for (k, v) in &report.params_after {
            println!("  {k} = {}", v.as_text());
        }
    }
    Ok(())
}

fn fmt_params(params: &std::collections::BTreeMap<String, tiered_memory::ParamValue>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_text()))
        .collect();
    format!(" {{{}}}", pairs.join(", "))
}

// -- credentials ---------------------------------------------------------------

fn credentials() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    match args.first().map(String::as_str) {
        // bare `credentials` = the common path: interactive setup
        None | Some("set") => credentials_set(&args),
        Some("show") => credentials_show(),
        Some("clear") => credentials_clear(),
        Some(other) => Err(format!(
            "unknown credentials subcommand `{other}` (setup | show | clear)"
        )),
    }
}

fn credentials_set(args: &[String]) -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    let has_flags = arg_value(args, "--base-url").is_some()
        || arg_value(args, "--api-key").is_some()
        || arg_value(args, "--model").is_some()
        || arg_value(args, "--temperature").is_some();

    let mut config = tiered_memory::LlmConfig::load_from(&path)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    if has_flags {
        // scriptable path — exactly what the flags say, nothing else
        if let Some(v) = arg_value(args, "--base-url") {
            config.base_url = v;
        }
        if let Some(v) = arg_value(args, "--api-key") {
            config.api_key = Some(v);
        }
        if let Some(v) = arg_value(args, "--model") {
            config.model = v;
        }
        if let Some(v) = arg_value(args, "--temperature") {
            config.temperature = v.parse::<f32>().ok();
        }
    } else if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        // interactive wizard: provider → key → searchable model list
        if !tiered_memory::tui::run_credentials_wizard(&mut config).map_err(|e| e.to_string())? {
            println!("cancelled — credentials unchanged");
            return Ok(());
        }
    } else {
        return Err("no TTY — pass --base-url/--api-key/--model or run from a terminal".into());
    }

    config.save_to(&path).map_err(|e| e.to_string())?;
    println!("credentials written to {}", path.display());
    println!("  base_url: {}", config.base_url);
    println!("  model:    {}", config.model);
    println!(
        "  api_key:  {}",
        tiered_memory::llm::mask_key(config.api_key.as_deref().unwrap_or("(none)"))
    );
    Ok(())
}

fn credentials_show() -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    let config = tiered_memory::LlmConfig::resolve(None, &data_root()).map_err(|e| e.to_string())?;
    match config {
        Some(c) => {
            let source = if path.is_file() {
                path.display().to_string()
            } else {
                "environment (TM_LLM_*)".to_string()
            };
            println!("LLM credentials (from {source}):");
            println!("  base_url: {}", c.base_url);
            println!("  model:    {}", c.model);
            println!(
                "  api_key:  {}",
                tiered_memory::llm::mask_key(c.api_key.as_deref().unwrap_or("(none)"))
            );
        }
        None => {
            println!("no LLM credentials configured.");
            println!("run the interactive setup with:");
            println!("  tiered-memory credentials");
            println!("or non-interactively:");
            println!(
                "  tiered-memory credentials set --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini"
            );
            println!("(env vars TM_LLM_BASE_URL / TM_LLM_API_KEY / TM_LLM_MODEL also work)");
        }
    }
    Ok(())
}

fn credentials_clear() -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    match std::fs::remove_file(&path) {
        Ok(_) => println!("credentials removed ({})", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no credentials file at {}", path.display())
        }
        Err(e) => return Err(format!("remove {}: {e}", path.display())),
    }
    Ok(())
}

// -- auth ---------------------------------------------------------------------

/// `tiered-memory auth on|off|show` — bearer-token auth for the HTTP service.
/// `on` generates a high-entropy token, stores it at `{data}/token` (0600)
/// and prints it once; `serve` enforces it on next start.
fn auth() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let sub = args.first().map(String::as_str).unwrap_or("show");
    let path = data_root().join("token");

    match sub {
        "on" => {
            let mut buf = [0u8; 32];
            getrandom::fill(&mut buf).map_err(|e| format!("entropy source: {e}"))?;
            let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            #[cfg(unix)]
            {
                use std::io::Write as _;
                use std::os::unix::fs::OpenOptionsExt as _;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(|e| format!("write {}: {e}", path.display()))?;
                f.write_all(token.as_bytes())
                    .map_err(|e| format!("write {}: {e}", path.display()))?;
            }
            #[cfg(not(unix))]
            std::fs::write(&path, &token).map_err(|e| format!("write {}: {e}", path.display()))?;
            println!(
                "auth ON — token written to {} (perms 0600, shown once):",
                path.display()
            );
            println!("  {token}");
            println!("clients send: Authorization: Bearer <token>");
            println!("restart `tiered-memory serve` to enforce it");
        }
        "off" => match std::fs::remove_file(&path) {
            Ok(_) => println!("auth OFF — token removed ({})", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("auth already OFF — no token file at {}", path.display())
            }
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        },
        _ => match std::fs::read_to_string(&path) {
            Ok(t) => {
                let t = t.trim();
                println!(
                    "auth ON — token {} in {}",
                    tiered_memory::llm::mask_key(t),
                    path.display()
                );
                println!("clients send: Authorization: Bearer <token>; restart serve after changes");
            }
            Err(_) => {
                println!("auth OFF — no token file at {}", path.display());
                println!("turn it on with: tiered-memory auth on");
            }
        },
    }
    Ok(())
}

/// `tiered-memory models` — list the configured provider's model catalog
/// (the same data the wizard's searchable picker shows).
fn models() -> Result<(), String> {
    let config = tiered_memory::LlmConfig::resolve(None, &data_root())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no LLM credentials — run `tiered-memory credentials` first".to_string())?;
    let list = tiered_memory::llm::fetch_models(&config.base_url, config.api_key.as_deref())
        .map_err(|e| e.to_string())?;
    println!("models at {} ({}):", config.base_url, list.len());
    for m in &list {
        let cur = if *m == config.model { "  ← current" } else { "" };
        println!("  {m}{cur}");
    }
    Ok(())
}

// -- install-skill -----------------------------------------------------------

/// Install the bundled `/tiered-memory` agent skill into a harness skills
/// directory. Overwrites any previous copy — re-run after upgrading.
fn install_skill() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let default_dir = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".agents").join("skills"))
        .unwrap_or_else(|_| PathBuf::from(".agents/skills"));
    let dir = arg_value(&args, "--dir")
        .map(PathBuf::from)
        .unwrap_or(default_dir);
    let target = dir.join("tiered-memory");
    std::fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
    let skill = include_str!("../../skill/SKILL.md");
    let path = target.join("SKILL.md");
    std::fs::write(&path, skill).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("skill installed: {}", path.display());
    println!("the `/tiered-memory` skill is now available to harnesses that load ~/.agents/skills (restart/refresh the harness if it caches its skill list)");
    Ok(())
}

// -- env ---------------------------------------------------------------------

/// `tiered-memory env` — emit-and-eval exports.
///
/// A process cannot modify its parent shell's environment (the env is copied
/// at fork and never propagated back), so instead of exporting, this prints
/// shell code the caller applies with `eval "$(tiered-memory env)"` — the
/// same pattern used by direnv-style tools. Prints the binary's own directory
/// for PATH (found via /proc self-exe, not $0) and the resolved data dir.
fn print_env() -> Result<(), String> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            println!("export PATH={}:$PATH", shell_quote(&dir.to_string_lossy()));
        }
    }
    println!("export TM_DATA_DIR={}", shell_quote(&data_root().to_string_lossy()));
    Ok(())
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
