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
//!
//! Module map: [`project`] (setup/init, projects, select, group, use) ·
//! [`memory`] (remember, feedback, recall, params, sync, stats) ·
//! [`credentials`] (LLM credentials, auth token, models) · [`service`]
//! (serve, env) · [`status`] (setup report) · [`skill`] (install-skill,
//! console).

mod bench;
mod clean;
mod credentials;
mod hooks;
mod memory;
mod project;
mod service;
mod skill;
mod status;

use std::path::PathBuf;
use std::sync::Arc;
use tiered_memory::{
    default_data_dir, EmbedderConfig, EngineConfig, JsonFileStore, LayeredDirStore, MemoryEngine,
    MemoryStore, DEFAULT_BIND, LOCAL_USER,
};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("serve") => service::serve().await,
        Some("init") | Some("setup") => project::init(),
        Some("projects") => project::projects(),
        Some("select") => project::select(),
        Some("group") => project::group_cmd(),
        Some("use") => project::use_cmd(),
        Some("params") => memory::params(),
        Some("remember") => memory::remember(),
        Some("feedback") => memory::feedback_cmd(),
        Some("recall") => memory::recall(),
        Some("forget") => memory::forget_cmd(),
        Some("sync") => memory::sync_cmd(),
        Some("stats") => memory::stats(),
        Some("credentials") => credentials::credentials(),
        Some("auth") => credentials::auth(),
        Some("models") => credentials::models(),
        Some("install-skill") => skill::install_skill(),
        Some("install-hooks") => hooks::install_hooks_cmd(),
        Some("hook") => hooks::hook_cmd(),
        Some("bench") => bench::bench_cmd(),
        Some("clean") => clean::clean_cmd(),
        Some("console") => skill::console_cmd(),
        Some("env") => service::print_env(),
        Some("status") => status::status_cmd(),
        Some("help") | Some("--help") | Some("-h") => {
            print_usage();
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown command `{other}` — run `tiered-memory help`"
        )),
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
  tiered-memory setup | init [--name N] [--id ID] [--descriptor T]
                                         [--tag T]... [--component C]...
                                         [--group NAME|none] [--user U] [--gitignore]
                                         set THIS directory up as a project:
                                         register, ask its L2 group, offer LLM
                                         credentials and the /tiered-memory skill
                                         for your harness(es) (scripts: --group and
                                         --gitignore pre-answers the prompts;
                                         --tag/--component are repeatable and feed
                                         similar-project matching)
  tiered-memory projects [--user U]      list projects using tiered memory
  tiered-memory projects remove <id>     unregister a project + forget its records
  tiered-memory select  [--user U] [--project P]
                                         pick the current project (interactive without P)
  tiered-memory group [--project P] [--user U]
                                         show this project's L2 group (+ suggestion
                                         and the existing groups when unset);
                                         `group set <name|none>` assigns it (asked
                                         once per project by the skill);
                                         `group rename <old> <new>` moves every
                                         project + group-owned memory — renaming
                                         onto an existing group merges the two
  tiered-memory status [--user U] [--check]
                                         one-glance setup report: service, data
                                         dir, project resolution, L2 group, LLM
                                         credentials (--check exits 1 when this
                                         directory isn't set up, for
                                         `status --check || init`)
  tiered-memory use [--project P] [--user U]
                                         show this project's memory sources — and
                                         who is drawing on it
  tiered-memory use <other>              this project now also sees <other>'s L1+L2
                                         memories (its warm tier; never the reverse)
  tiered-memory use --remove <other>     stop drawing on <other>
  tiered-memory params  [--user U] [--project P]
                                         show the adjusted parameter set
  tiered-memory remember \"text\" [--project P] [--kind KIND]
                        [--level L1|L2|L3] [--group NAME] [--topic TOPIC] [--global]
                        [--param k=v]... [--pin] [--ttl DAYS] [--user U]
                                         store a memory (--group NAME: L2 memory
                                         owned by the whole group of projects;
                                         --topic: category slug filing L2 records
                                         into per-topic docs, e.g. writing-style)
  tiered-memory feedback <key> <value> [--global] [--project P]
                                         assert one learner parameter
                                         (the agent-facing signal API)
  tiered-memory recall \"query\" [--project P] [--k N] [--min F] [--user U]
                                         layered search
  tiered-memory forget <id> | --project P | --level L1|L2|L3 | --all [--yes]
                                         delete memories (the right to be
                                         forgotten — what was remembered can
                                         be un-remembered; --all confirms)
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
  tiered-memory install-skill            install the /tiered-memory agent skill
                                         (--harness <id,id> or --dir D skip the
                                         picker; --subcommands adds completable
                                         /tiered-memory-* skills; --list shows
                                         harnesses + how to add your own)
  tiered-memory install-hooks [--harness <id,id>] [--remove]
                                         wire memory into session start/end
                                         (session-start injects the learner
                                         brief; session-end syncs the transcript)
  tiered-memory hook session-start | session-end
                                         the command the hooks run — you rarely
                                         call this by hand
  tiered-memory console [--user U]       terminal dashboard: layers, projects,
                                         L2 groups + per-topic docs, installed
                                         skills, project↔group graph
  tiered-memory clean [--skills | --data] [--yes]
                                         remove everything tiered-memory created:
                                         the data dir (memories + LLM credentials)
                                         and installed agent skill copies — asks
                                         for confirmation unless --yes
  tiered-memory bench [--projects N] [--per-project N] [--queries N]
                      [--embedder hashing|local|http] [--keep] [--json]
                                         benchmark write/recall/consolidate on a
                                         deterministic synthetic corpus in a
                                         scratch store (real data is never touched);
                                         compare environments via TM_EMBEDDER /
                                         TM_DATA_DIR and diff the tables
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

// -- shared environment helpers ----------------------------------------------

/// The command line after the subcommand (`argv[2..]`).
pub(crate) fn cmd_args() -> Vec<String> {
    std::env::args().skip(2).collect()
}

pub(crate) fn data_root() -> PathBuf {
    std::env::var("TM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_data_dir())
}

pub(crate) fn default_user() -> String {
    std::env::var("TM_USER").unwrap_or_else(|_| LOCAL_USER.to_string())
}

pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub(crate) fn store() -> Result<Arc<dyn MemoryStore>, String> {
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

/// A memory engine over the local store with the embedder from the env —
/// the fallback when no `tiered-memory serve` is running.
pub(crate) fn local_engine() -> Result<MemoryEngine, String> {
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

// -- arg parsing -------------------------------------------------------------

/// Value of `--flag <value>` from the arg list.
pub(crate) fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Every value of a repeatable `--flag <value>` (`--tag a --tag b` → `[a, b]`).
pub(crate) fn arg_values(args: &[String], flag: &str) -> Vec<String> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1].clone())
        .collect()
}

/// Boolean switch `--flag`.
pub(crate) fn arg_switch(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Flags whose next argument is a value (skipped by positional scanning).
const VALUE_FLAGS: &[&str] = &[
    "--user",
    "--project",
    "--name",
    "--id",
    "--descriptor",
    "--kind",
    "--param",
    "--ttl",
    "--k",
    "--min",
    "--level",
    "--group",
    "--topic",
    "--tag",
    "--component",
];

fn takes_value(flag: &str) -> bool {
    VALUE_FLAGS.contains(&flag)
}

/// First argument that is not a flag or a flag value.
pub(crate) fn positional(args: &[String]) -> Option<String> {
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with('-') {
            if takes_value(a) {
                skip_next = true;
            }
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Collect all non-flag arguments (flag values excluded).
pub(crate) fn positional_list(args: &[String]) -> Vec<String> {
    let mut skip_next = false;
    let mut out = Vec::new();
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a.starts_with('-') {
            if takes_value(a) {
                skip_next = true;
            }
            continue;
        }
        out.push(a.clone());
    }
    out
}

/// `(user, explicit --project)` — the flag pair most commands share.
pub(crate) fn flags(args: &[String]) -> (String, Option<String>) {
    (
        arg_value(args, "--user").unwrap_or_else(default_user),
        arg_value(args, "--project"),
    )
}

/// Explicit `--project`, else the `select` marker.
pub(crate) fn resolve_project(args: &[String], user: &str) -> Result<String, String> {
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
    current_selection(user).ok_or(
        "no --project given, no tiered-memory.json here, and no selection — run `tiered-memory init` or `select`".into(),
    )
}

pub(crate) fn current_selection(user: &str) -> Option<String> {
    LayeredDirStore::new(data_root())
        .ok()?
        .current_project(user)
        .ok()
        .flatten()
}

// -- HTTP-first helper -------------------------------------------------------

pub(crate) enum Api {
    Ok(serde_json::Value),
    Err(String),
    Unreachable,
}

fn api_base() -> String {
    std::env::var("TM_BASE_URL").unwrap_or_else(|_| format!("http://{DEFAULT_BIND}"))
}

/// Attach the service bearer token, if one is on disk.
fn with_service_auth<S>(req: ureq::RequestBuilder<S>) -> ureq::RequestBuilder<S> {
    if let Ok(token) = std::fs::read_to_string(data_root().join("token")) {
        let t = token.trim();
        if !t.is_empty() {
            return req.header("Authorization", &format!("Bearer {t}"));
        }
    }
    req
}

/// Classify a response from the running service. Statuses are not errors
/// (`http_status_as_error(false)`) so the error body can be surfaced.
fn api_response(mut resp: ureq::http::Response<ureq::Body>) -> Api {
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let msg = resp.body_mut().read_to_string().unwrap_or_default();
        return Api::Err(format!("service returned {status}: {msg}"));
    }
    match resp.body_mut().read_json::<serde_json::Value>() {
        Ok(v) => Api::Ok(v),
        Err(e) => Api::Err(format!("unreadable response from service: {e}")),
    }
}

/// POST to the running service, if there is one. `Err` means the service IS
/// reachable but rejected the call (surface it — don't silently fall back);
/// `Unreachable` lets the caller use the local store directly.
pub(crate) fn api_post(path: &str, body: &serde_json::Value) -> Api {
    let req = ureq::post(&format!("{}{}", api_base(), path))
        .config()
        .timeout_global(Some(std::time::Duration::from_millis(2000)))
        .http_status_as_error(false)
        .build();
    match with_service_auth(req).send_json(body) {
        Ok(resp) => api_response(resp),
        Err(_) => Api::Unreachable,
    }
}

/// GET counterpart of [`api_post`] — same auth, same Unreachable semantics.
/// Lets read-side callers (the session-start hook) see the running service's
/// store instead of stale local disk.
pub(crate) fn api_get(path: &str) -> Api {
    let req = ureq::get(&format!("{}{}", api_base(), path))
        .config()
        .timeout_global(Some(std::time::Duration::from_millis(2000)))
        .http_status_as_error(false)
        .build();
    match with_service_auth(req).call() {
        Ok(resp) => api_response(resp),
        Err(_) => Api::Unreachable,
    }
}

/// DELETE counterpart of [`api_get`] — same auth, same Unreachable semantics.
pub(crate) fn api_delete(path: &str) -> Api {
    let req = ureq::delete(&format!("{}{}", api_base(), path))
        .config()
        .timeout_global(Some(std::time::Duration::from_millis(2000)))
        .http_status_as_error(false)
        .build();
    match with_service_auth(req).call() {
        Ok(resp) => api_response(resp),
        Err(_) => Api::Unreachable,
    }
}

/// Run one command through the running service when it is up, otherwise apply
/// it locally. A reachable-but-rejecting service is never bypassed — `Err`
/// from the service propagates. `on_ok` and `local` own the success handling
/// (usually printing) and the returned value.
pub(crate) fn service_or_local<T>(
    path: &str,
    body: &serde_json::Value,
    on_ok: impl FnOnce(serde_json::Value) -> Result<T, String>,
    local: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    match api_post(path, body) {
        Api::Ok(v) => on_ok(v),
        Api::Err(e) => Err(e),
        Api::Unreachable => local(),
    }
}
