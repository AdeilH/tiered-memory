//! Harness hooks: `install-hooks` writes the session-start/end commands into
//! harness settings, and `hook` is the command they run. Hooks make memory
//! automatic — the learner brief enters context at session start and the
//! transcript is synced at session end, no skill invocation needed.
//!
//! Hook contracts (see `src/harnesses.rs` for where each config lives):
//! * **session-start**: print the memory brief to stdout. Claude Code adds
//!   SessionStart stdout to context; ZCode wants strict JSON
//!   (`--format json` → `additionalContext`). Always exits 0 — a memory
//!   failure must never break a session.
//! * **session-end**: read the harness's stdin JSON, sync the transcript
//!   through the LLM extraction pipeline. Best-effort: without credentials
//!   or a resolvable project it exits 0 silently.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{arg_switch, arg_value, cmd_args, data_root, default_user, home_dir, local_engine};

// -- install-hooks -----------------------------------------------------------

/// `tiered-memory install-hooks [--harness <id,id>] [--remove]` — write (or
/// with `--remove`, strip) the tiered-memory hook commands in the harnesses'
/// settings files. Defaults to every supported harness that is detected.
pub(crate) fn install_hooks_cmd() -> Result<(), String> {
    let args = cmd_args();
    let home = home_dir();
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let remove = arg_switch(&args, "--remove");

    let targets: Vec<&'static tiered_memory::harnesses::Harness> =
        match arg_value(&args, "--harness") {
            Some(spec) => parse_ids(&spec)?,
            None => tiered_memory::harnesses::registry(&home)
                .into_iter()
                .filter(|h| {
                    tiered_memory::harnesses::hook_support(h, &home).is_some()
                        && (remove || h.detected(&home))
                })
                .collect(),
        };
    if targets.is_empty() {
        return Err(
            "no harnesses support hooks — supported: claude, agents (ZCode); pass --harness explicitly".into(),
        );
    }

    for h in targets {
        let outcome = if remove {
            tiered_memory::harnesses::remove_hooks(h, &home, &cwd)
        } else {
            tiered_memory::harnesses::install_hooks(h, &home, &cwd)
        };
        match outcome {
            Ok(Some(path)) => {
                if remove {
                    println!("hooks removed: {} ({})", path.display(), h.label);
                } else {
                    println!("hooks installed: {} ({})", path.display(), h.label);
                }
            }
            Ok(None) => {
                if remove {
                    println!("no tiered-memory hooks found: {}", h.label);
                }
            }
            Err(e) => eprintln!("tiered-memory: {} — {e}", h.label),
        }
    }
    if !remove {
        println!("restart the harness to pick up the new hooks");
    }
    Ok(())
}

fn parse_ids(spec: &str) -> Result<Vec<&'static tiered_memory::harnesses::Harness>, String> {
    let reg = tiered_memory::harnesses::registry(&home_dir());
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|id| {
            reg.iter().copied().find(|h| h.id == id).ok_or_else(|| {
                format!("unknown harness `{id}` — see `tiered-memory install-skill --list`")
            })
        })
        .collect()
}

// -- hook command ------------------------------------------------------------

/// `tiered-memory hook session-start | session-end` — what the harnesses'
/// hook configs invoke. Errors never fail the session: everything prints to
/// stderr and exits 0.
pub(crate) fn hook_cmd() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    match args.first().map(String::as_str) {
        Some("session-start") => session_start(&args, &user),
        Some("session-end") => session_end(&args, &user),
        other => Err(format!(
            "unknown hook event `{other:?}` — use session-start | session-end"
        )),
    }
}

/// Drain the harness's stdin (the hook input JSON) so the writer never sees
/// a broken pipe; we don't need its contents at start.
fn drain_stdin() {
    let mut buf = String::new();
    let mut stdin = std::io::stdin().lock();
    let _ = stdin.read_to_string(&mut buf);
}

/// Resolve the project from the cwd (the marker file), tolerating absence.
fn project_in(dir: &Path, user: &str) -> Option<String> {
    let marker = std::fs::read_to_string(dir.join("tiered-memory.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&marker).ok()?;
    if v.get("user").and_then(|x| x.as_str()) != Some(user) {
        return None;
    }
    v.get("project_id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
}

fn session_start(args: &[String], user: &str) -> Result<(), String> {
    drain_stdin();
    let brief = build_brief(args, user).unwrap_or_default();
    let json = arg_switch(args, "--format");
    if json {
        // ZCode parses stdout as strict JSON; additionalContext reaches the model
        let out = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": brief,
            }
        });
        println!("{out}");
    } else if !brief.is_empty() {
        println!("{brief}");
    }
    Ok(())
}

/// The learner brief injected at session start: adjusted parameters, the L2
/// group, and the most relevant memories. Empty when there is nothing yet.
/// Reads the running service first (hooks must see the same store the CLI
/// writes into), falling back to the local store when none is reachable.
fn build_brief(args: &[String], user: &str) -> Option<String> {
    // an explicit --project wins; otherwise the cwd's marker decides
    let project = arg_value(args, "--project")
        .or_else(|| project_in(&std::env::current_dir().ok()?, user))?;
    if let Some(brief) = brief_over_http(user, &project) {
        return Some(brief);
    }

    let engine = local_engine().ok()?;
    let ctx = engine.memory_context(user, Some(&project), 8).ok()?;
    render_brief(
        &project,
        ctx.group.as_deref(),
        &ctx.params
            .iter()
            .map(|p| (p.key.clone(), p.value.as_text()))
            .collect::<Vec<_>>(),
        &ctx.l1
            .iter()
            .chain(&ctx.l2)
            .chain(&ctx.l3)
            .take(8)
            .map(|m| m.text.clone())
            .collect::<Vec<_>>(),
    )
}

/// Same brief from `GET /v1/context/{user}/{project}` — the service's store.
fn brief_over_http(user: &str, project: &str) -> Option<String> {
    let v = match crate::api_get(&format!("/v1/context/{user}/{project}")) {
        crate::Api::Ok(v) => v,
        _ => return None,
    };
    let group = v["group"].as_str().map(str::to_string);
    let params: Vec<(String, String)> = v["params"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| Some((p["key"].as_str()?.to_string(), fmt_value(&p["value"]))))
                .collect()
        })
        .unwrap_or_default();
    let memories: Vec<String> = ["l1", "l2", "l3"]
        .iter()
        .filter_map(|k| v[*k].as_array())
        .flatten()
        .filter_map(|m| m["text"].as_str().map(str::to_string))
        .take(8)
        .collect();
    render_brief(project, group.as_deref(), &params, &memories)
}

/// ParamValue arrives as raw JSON (number | string | bool) — render it the
/// way `params` does (integers without a trailing ".0").
fn fmt_value(v: &serde_json::Value) -> String {
    if let Some(n) = v.as_f64() {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return format!("{}", n as i64);
        }
        return format!("{n}");
    }
    if let Some(b) = v.as_bool() {
        return b.to_string();
    }
    v.as_str().unwrap_or("?").to_string()
}

fn render_brief(
    project: &str,
    group: Option<&str>,
    params: &[(String, String)],
    memories: &[String],
) -> Option<String> {
    let mut out = String::new();
    if let Some(g) = group {
        out.push_str(&format!("L2 group: {g}\n"));
    }
    if !params.is_empty() {
        out.push_str("learner parameters: ");
        out.push_str(
            &params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }
    if !memories.is_empty() {
        out.push_str("remembered:\n");
        for m in memories {
            out.push_str(&format!("- {m}\n"));
        }
    }
    (!out.is_empty()).then(|| format!("# tiered-memory brief (project: {project})\n{out}"))
}

fn session_end(args: &[String], user: &str) -> Result<(), String> {
    // Claude Code's SessionEnd input: {"transcript_path": …, "cwd": …, …}
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let parsed: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::json!({}));
    let transcript_path = parsed
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| arg_value(args, "--transcript").map(PathBuf::from));
    let cwd: PathBuf = parsed
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let Some(project) = arg_value(args, "--project").or_else(|| project_in(&cwd, user)) else {
        return Ok(()); // not a tiered-memory project — nothing to do
    };
    let Some(path) = transcript_path else {
        return Ok(());
    };
    let Ok(conversation) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    if conversation.trim().is_empty() {
        return Ok(());
    }

    let config = match tiered_memory::LlmConfig::resolve(None, &data_root()) {
        Ok(Some(c)) => c,
        _ => return Ok(()), // no LLM — sync is unavailable, stay silent
    };
    let llm = match tiered_memory::LlmClient::new(config) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("tiered-memory hook: {e}");
            return Ok(());
        }
    };
    let engine = match local_engine() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tiered-memory hook: {e}");
            return Ok(());
        }
    };
    let input = tiered_memory::SyncInput {
        user: user.to_string(),
        project_id: project.clone(),
        conversation,
    };
    match tiered_memory::plan(&engine, &llm, &input) {
        Ok(plan) => {
            if plan.entries.is_empty() {
                return Ok(());
            }
            match crate::memory::apply_plan(&engine, &user, &project, &plan) {
                Ok(report) => eprintln!(
                    "tiered-memory hook: synced {} memories into `{project}`",
                    report.stored.len()
                ),
                Err(e) => eprintln!("tiered-memory hook: {e}"),
            }
        }
        Err(e) => eprintln!("tiered-memory hook: extraction skipped ({e})"),
    }
    Ok(())
}
