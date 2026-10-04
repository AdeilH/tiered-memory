//! Memory commands: `remember`, `feedback`, `recall`, `params`, `sync`, and
//! `stats` — the day-to-day surface agents and scripts call.

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};

use crate::{
    arg_switch, arg_value, cmd_args, data_root, default_user, flags, local_engine, positional,
    positional_list, resolve_project, service_or_local, store,
};
use tiered_memory::{EngineConfig, MemoryEngine, ParamValue};

// -- remember ----------------------------------------------------------------

fn parse_param(s: &str) -> Result<(String, ParamValue), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("--param expects key=value, got `{s}`"))?;
    Ok((k.to_string(), coerce_value(v)))
}

/// Coerce a CLI string into a ParamValue: number → Number, true/false → Bool, else Text.
fn coerce_value(v: &str) -> ParamValue {
    if let Ok(n) = v.parse::<f64>() {
        ParamValue::Number(n)
    } else if v == "true" || v == "false" {
        ParamValue::Bool(v == "true")
    } else {
        ParamValue::Text(v.to_string())
    }
}

/// All `--param k=v` occurrences, as a structured map.
fn collect_params(args: &[String]) -> Result<BTreeMap<String, ParamValue>, String> {
    let mut params = BTreeMap::new();
    for pair in args.windows(2).filter(|w| w[0] == "--param") {
        let (k, v) = parse_param(&pair[1])?;
        params.insert(k, v);
    }
    Ok(params)
}

pub(crate) fn remember() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let text = positional(&args).ok_or("give the memory as a quoted argument")?;
    let global = arg_switch(&args, "--global");
    let level = parse_level(&args)?;
    // `--group <name>` writes an L2 record owned by the whole group of
    // projects (no single project owns it); `--level L2` alone stays
    // project-owned and surfaces to group-mates via membership.
    let group = arg_value(&args, "--group");
    if group.is_some() && (global || arg_value(&args, "--project").is_some()) {
        return Err(
            "--group writes a group-owned memory — it cannot be combined with --global or --project"
                .into(),
        );
    }
    if group.is_some()
        && matches!(
            level,
            Some(tiered_memory::Level::L1 | tiered_memory::Level::L3)
        )
    {
        return Err("--group is L2 — drop the --level flag or set it to L2".into());
    }
    let level = level.or_else(|| group.is_some().then_some(tiered_memory::Level::L2));
    let topic = arg_value(&args, "--topic");
    let project = if global || group.is_some() {
        None
    } else {
        Some(resolve_project(&args, &user)?)
    };
    let params = collect_params(&args)?;
    let pinned = arg_switch(&args, "--pin");
    let ttl_days = arg_value(&args, "--ttl").and_then(|t| t.parse::<f64>().ok());

    let body = serde_json::json!({
        "user": user,
        "text": text,
        "project_id": project,
        "level": level,
        "group": group,
        "topic": topic,
        "params": if params.is_empty() { None } else { Some(params.clone()) },
        "pinned": pinned,
        "ttl_days": ttl_days,
    });
    service_or_local(
        "/v1/remember",
        &body,
        |out| {
            println!(
                "stored `{}` (deduped: {})",
                out["id"].as_str().unwrap_or("?"),
                out["deduped"].as_bool().unwrap_or(false)
            );
            Ok(())
        },
        move || {
            let out = local_engine()?
                .remember(tiered_memory::RememberInput {
                    user,
                    text,
                    project_id: project,
                    kind: None,
                    params: Some(params),
                    key_hint: None,
                    confidence: None,
                    pinned: Some(pinned),
                    ttl_days,
                    level,
                    group,
                    topic,
                })
                .map_err(|e| e.to_string())?;
            println!("stored `{}` (local store, no service running)", out.id);
            Ok(())
        },
    )
}

/// `--level L1|L2|L3` (case-insensitive, bare digit accepted).
fn parse_level(args: &[String]) -> Result<Option<tiered_memory::Level>, String> {
    match arg_value(args, "--level").as_deref() {
        None => Ok(None),
        Some(l) => Ok(Some(match l.to_ascii_uppercase().as_str() {
            "L1" | "1" => tiered_memory::Level::L1,
            "L2" | "2" => tiered_memory::Level::L2,
            "L3" | "3" => tiered_memory::Level::L3,
            other => return Err(format!("invalid --level `{other}` (L1 | L2 | L3)")),
        })),
    }
}

// -- feedback ----------------------------------------------------------------

/// `tiered-memory feedback <key> <value> [--global]` — assert one learner
/// parameter. The primary agent-facing signal API: agents call this the moment
/// a learning signal happens (checkpoint outcome, pace complaint, preference).
pub(crate) fn feedback_cmd() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let positionals = positional_list(&args);
    let (key, value) = match (positionals.first(), positionals.get(1)) {
        (Some(k), Some(v)) => (k.clone(), coerce_value(v)),
        _ => return Err("usage: tiered-memory feedback <key> <value> [--global]".into()),
    };
    let global = arg_switch(&args, "--global");
    let project = if global {
        None
    } else {
        Some(resolve_project(&args, &user)?)
    };

    let body = serde_json::json!({
        "user": user,
        "key": key,
        "value": value,
        "project_id": project,
        "global": global,
    });
    // the success printer needs the values too — give the closures their own
    // copies so the local fallback can move them into FeedbackInput
    let (ok_key, ok_value) = (key.clone(), value.clone());
    service_or_local(
        "/v1/feedback",
        &body,
        move |out| {
            println!(
                "recorded {ok_key} = {} (deduped: {})",
                ok_value.as_text(),
                out["deduped"].as_bool().unwrap_or(false)
            );
            Ok(())
        },
        move || {
            local_engine()?
                .feedback(tiered_memory::FeedbackInput {
                    user,
                    key,
                    value,
                    project_id: project,
                    weight: None,
                    global: Some(global),
                })
                .map_err(|e| e.to_string())?;
            println!("recorded (local store, no service running)");
            Ok(())
        },
    )
}

// -- recall ------------------------------------------------------------------

pub(crate) fn recall() -> Result<(), String> {
    let args = cmd_args();
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
    let out = crate::service_or_local("/v1/recall", &body, Ok, || {
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
        serde_json::to_value(&out).map_err(|e| e.to_string())
    })?;

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

pub(crate) fn params() -> Result<(), String> {
    let args = cmd_args();
    let (user, explicit) = flags(&args);
    let project = match explicit {
        Some(p) => p,
        None => resolve_project(&args, &user)?,
    };

    let engine = local_engine()?;
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
pub(crate) fn sync_cmd() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let project = resolve_project(&args, &user)?;
    let conversation = read_conversation(&args)?;

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
            println!(
                "  {} {}{}",
                fmt_sync_level(e),
                e.text,
                fmt_params(&e.params)
            );
        }
        return Ok(());
    }

    let report = apply_plan(&engine, &user, &project, &plan)?;
    print_sync_report(&user, &project, &report);
    Ok(())
}

/// `--stdin`, `--file <path>` or the quoted positional — wherever the
/// transcript comes from.
fn read_conversation(args: &[String]) -> Result<String, String> {
    if arg_switch(args, "--stdin") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("read stdin: {e}"))?;
        return Ok(buf);
    }
    if let Some(path) = arg_value(args, "--file") {
        return std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"));
    }
    positional(args)
        .ok_or("give the conversation via --file <path>, --stdin, or as a quoted argument".into())
}

/// Write each planned entry through the service when it is up, else through
/// the SAME local engine (so its cache stays coherent for params_after).
/// Shared by `sync` and the session-end hook.
pub(crate) fn apply_plan(
    engine: &MemoryEngine,
    user: &str,
    project: &str,
    plan: &tiered_memory::SyncPlan,
) -> Result<tiered_memory::SyncReport, String> {
    let mut report = tiered_memory::SyncReport {
        raw_model_reply: plan.raw_model_reply.clone(),
        ..Default::default()
    };
    for entry in &plan.entries {
        let body = serde_json::json!({
            "user": user,
            "text": entry.text,
            "project_id": if entry.level == tiered_memory::Level::L3 { None } else { Some(project) },
            "level": entry.level,
            "params": if entry.params.is_empty() { None } else { Some(entry.params.clone()) },
            "key_hint": entry.key,
            "confidence": entry.confidence,
        });
        match crate::api_post("/v1/remember", &body) {
            crate::Api::Ok(_) => report.stored.push(entry.clone()),
            crate::Api::Err(e) => report.skipped.push(e),
            crate::Api::Unreachable => {
                match engine.remember(tiered_memory::RememberInput {
                    user: user.to_string(),
                    text: entry.text.clone(),
                    project_id: if entry.level == tiered_memory::Level::L3 {
                        None
                    } else {
                        Some(project.to_string())
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
                    group: None,
                    topic: entry.topic.clone(),
                }) {
                    Ok(_) => report.stored.push(entry.clone()),
                    Err(e) => report.skipped.push(format!("{} ({e})", entry.text)),
                }
            }
        }
    }
    for s in engine
        .adjusted_parameters(user, Some(project))
        .map_err(|e| e.to_string())?
    {
        report.params_after.insert(s.key, s.value);
    }
    Ok(report)
}

fn print_sync_report(user: &str, project: &str, report: &tiered_memory::SyncReport) {
    println!(
        "sync for `{user}` · `{project}`: {} stored, {} skipped",
        report.stored.len(),
        report.skipped.len()
    );
    for e in &report.stored {
        println!(
            "  {} {}{}",
            fmt_sync_level(e),
            e.text,
            fmt_params(&e.params)
        );
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
}

/// `[L2 · writing-style]`-style layer/topic prefix for sync plan/report lines.
fn fmt_sync_level(e: &tiered_memory::SyncEntry) -> String {
    match &e.topic {
        Some(t) => format!("[{:?} · {t}]", e.level),
        None => format!("[{:?}]", e.level),
    }
}

fn fmt_params(params: &BTreeMap<String, ParamValue>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let pairs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{k}={}", v.as_text()))
        .collect();
    format!(" {{{}}}", pairs.join(", "))
}

// -- forget ------------------------------------------------------------------

/// `tiered-memory forget` — delete memories. Exactly one target:
/// `<id>` positional, `--project P`, `--level L1|L2|L3`, or `--all`
/// (which asks for confirmation; scripts add `--yes`). This is the command
/// behind "never store secrets" — what was remembered can be un-remembered.
pub(crate) fn forget_cmd() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let id = positional(&args);
    let project = arg_value(&args, "--project");
    let level = parse_level(&args)?;
    let all = arg_switch(&args, "--all");
    let assume_yes = arg_switch(&args, "--yes");

    let targets = [id.is_some(), project.is_some(), level.is_some(), all];
    if targets.iter().filter(|t| **t).count() != 1 {
        return Err(
            "usage: tiered-memory forget <id> | forget --project P | forget --level L1|L2|L3 | forget --all [--yes]"
                .into(),
        );
    }
    if all && !assume_yes {
        if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
            return Err(
                "forget --all deletes every memory — pass --yes to confirm in scripts".into(),
            );
        }
        print!("delete ALL memories for `{user}`? This cannot be undone. [y/N] ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("forget cancelled — nothing was removed");
            return Ok(());
        }
    }

    let body = serde_json::json!({
        "user": user,
        "id": id,
        "project_id": project,
        "level": level,
        "all": all.then_some(true),
    });
    service_or_local(
        "/v1/forget",
        &body,
        |out| {
            println!(
                "forgot {} {}",
                out.as_u64().unwrap_or(0),
                plural(out.as_u64().unwrap_or(0))
            );
            Ok(())
        },
        || {
            let n = local_engine()?
                .forget(tiered_memory::ForgetInput {
                    user: user.clone(),
                    id,
                    project_id: project,
                    level,
                    all: all.then_some(true),
                })
                .map_err(|e| e.to_string())?;
            println!("forgot {n} {}", plural(n as u64));
            Ok(())
        },
    )
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        "memory"
    } else {
        "memories"
    }
}

// -- stats -------------------------------------------------------------------

pub(crate) fn stats() -> Result<(), String> {
    let args = cmd_args();
    let (user, _) = flags(&args);
    let db = store()?
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
