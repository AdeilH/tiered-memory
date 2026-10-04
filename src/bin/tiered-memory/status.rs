//! `tiered-memory status` — one-glance setup report and diagnostics. The
//! /tiered-memory skill starts here: anything missing → run `init` (which is
//! non-interactive when its prompts can't be answered), then continue.

use crate::{
    api_get, arg_switch, arg_value, cmd_args, current_selection, data_root, default_user, store,
    Api,
};
use tiered_memory::{LlmConfig, NO_GROUP, DEFAULT_BIND};

pub(crate) fn status_cmd() -> Result<(), String> {
    let args = cmd_args();
    let check = arg_switch(&args, "--check");
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);

    println!("tiered-memory {} — status", env!("CARGO_PKG_VERSION"));

    let base = std::env::var("TM_BASE_URL").unwrap_or_else(|_| format!("http://{DEFAULT_BIND}"));
    match api_get("/v1/health") {
        Api::Ok(v) => println!(
            "service:   reachable at {base} (v{}, embedder {})",
            v["version"].as_str().unwrap_or("?"),
            v["embedder"].as_str().unwrap_or("?")
        ),
        Api::Err(e) => println!("service:   reachable at {base} but errored: {e}"),
        Api::Unreachable => {
            println!("service:   not running at {base} — CLI writes go to the local store")
        }
    }
    println!("data dir:  {}", data_root().display());

    // how this directory resolves to a project (same rule as every command)
    let marker = std::fs::read_to_string("tiered-memory.json")
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v.get("user").and_then(|x| x.as_str()) == Some(user.as_str()))
        .and_then(|v| {
            v.get("project_id")
                .and_then(|x| x.as_str())
                .map(str::to_string)
        });
    let selection = current_selection(&user);
    let db = store()?.load(&user).map_err(|e| e.to_string())?;

    let mut set_up = false;
    match (&marker, &selection) {
        (Some(pid), _) if db.as_ref().is_some_and(|d| d.projects.contains_key(pid)) => {
            let p = &db.as_ref().expect("checked above").projects[pid.as_str()];
            println!("project:   `{pid}` ({}) ← tiered-memory.json here", p.name);
            match p.group.as_deref() {
                Some(NO_GROUP) => println!("L2 group:  none (confirmed)"),
                Some(g) => println!("L2 group:  {g}"),
                None => println!("L2 group:  (unset — `tiered-memory group` to pick one)"),
            }
            set_up = true;
        }
        (Some(pid), _) => {
            println!(
                "project:   marker here points at `{pid}`, but it is not registered — re-run `tiered-memory init`"
            )
        }
        (None, Some(sel)) => {
            println!(
                "project:   `{sel}` (via selection — no marker in this directory; run `tiered-memory init` here for automatic resolution)"
            );
            set_up = true;
        }
        (None, None) => {
            println!("project:   not set up for this directory — run `tiered-memory init`")
        }
    }

    match db {
        Some(db) => println!(
            "store:     {} project(s), {} memories for user `{user}`",
            db.projects.len(),
            db.records.len()
        ),
        None => println!("store:     empty for user `{user}`"),
    }
    match LlmConfig::resolve(None, &data_root()).map_err(|e| e.to_string())? {
        Some(cfg) => println!("llm:       configured ({})", cfg.model),
        None => println!("llm:       not configured — `tiered-memory credentials` (needed for `sync`)"),
    }

    if set_up {
        println!("\nset up for this directory — good to go.");
        Ok(())
    } else {
        println!("\nnot set up — run `tiered-memory init` in this directory.");
        if check {
            // `tiered-memory status --check || tiered-memory init`
            std::process::exit(1);
        }
        Ok(())
    }
}
