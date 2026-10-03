//! Project lifecycle commands: `setup`/`init`, `projects`, `select`, and
//! `group` (L2 group management).

use std::io::{BufRead, Write};
use std::path::Path;

use crate::{
    api_base, arg_switch, arg_value, cmd_args, data_root, default_user, flags, home_dir,
    local_engine, resolve_project, service_or_local, store,
};
use tiered_memory::{LayeredDirStore, ProjectInfo, ProjectInput, UserDb};

// -- init / setup ------------------------------------------------------------

/// Register the current directory as a project. Detects the project name and
/// description from `package.json`, `Cargo.toml`, `pyproject.toml` or the
/// directory name, registers it in the memory store (through the running
/// service when one is up), and writes a `tiered-memory.json` marker so hosts
/// and agents can discover how to reach this project's memory.
///
/// This is also the first-run wizard: it offers LLM credentials (needed by
/// `sync`) and the `/tiered-memory` skill for the user's harnesses — each
/// prompt once, skippable, never forced.
pub(crate) fn init() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;

    let project_id = register_current_project(&args, &user, &cwd)?;
    write_marker(&cwd, &user, &project_id)?;
    offer_gitignore(&args, &cwd)?;
    offer_credentials()?;
    offer_skill_install(&cwd)?;

    println!("\nnext:");
    println!("  tiered-memory remember \"prefers analogies from games\" --project {project_id}");
    println!("  tiered-memory recall \"how should I explain recursion?\" --project {project_id}");
    println!("  tiered-memory params --project {project_id}");
    println!("  tiered-memory console");
    println!("anywhere in this project: tiered-memory select --project {project_id}");
    Ok(())
}

/// Detect, register (HTTP-first) and echo the project; returns its id.
fn register_current_project(args: &[String], user: &str, cwd: &Path) -> Result<String, String> {
    let (detected_name, detected_desc) = detect_project(cwd);
    let name = arg_value(args, "--name").unwrap_or(detected_name.clone());
    let descriptor = arg_value(args, "--descriptor")
        .or_else(|| (!detected_desc.is_empty()).then_some(detected_desc.clone()))
        .unwrap_or_else(|| name.clone());
    let project_id = match arg_value(args, "--id") {
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
    let via = service_or_local(
        "/v1/projects",
        &body,
        |_| Ok("running service".to_string()),
        || {
            local_engine()?
                .register_project(ProjectInput {
                    user: user.to_string(),
                    project_id: project_id.clone(),
                    name: Some(name.clone()),
                    tags: vec![],
                    components: vec![],
                    descriptor: Some(descriptor.clone()),
                    group: None,
                })
                .map_err(|e| e.to_string())?;
            Ok("local store (no service running)".to_string())
        },
    )?;

    println!("registered `{project_id}` via {via}");
    if project_id != detected_name {
        println!("  ({detected_name} → {project_id})");
    }
    println!("memory data: {}", data_root().display());
    Ok(project_id)
}

/// Write the `tiered-memory.json` marker that resolves this directory to its
/// project on every later command.
fn write_marker(cwd: &Path, user: &str, project_id: &str) -> Result<(), String> {
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
    println!("wrote {}", config_path.display());
    Ok(())
}

/// Offer to keep the marker out of version control (dev-phase projects link
/// tiered-memory temporarily, so the default is yes).
fn offer_gitignore(args: &[String], cwd: &Path) -> Result<(), String> {
    let gitignore = cwd.join(".gitignore");
    let want_ignore = if arg_switch(args, "--gitignore") {
        true
    } else if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        print!("\nadd tiered-memory.json to .gitignore? [Y/n] ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        !matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no")
    } else {
        println!(
            "hint: add tiered-memory.json to .gitignore if you don't want it tracked (or re-run init with --gitignore)"
        );
        false
    };
    if want_ignore {
        match append_gitignore(&gitignore, "tiered-memory.json") {
            Ok(true) => println!("added to {}", gitignore.display()),
            Ok(false) => println!("already in {}", gitignore.display()),
            Err(e) => println!("could not update .gitignore: {e}"),
        }
    }
    Ok(())
}

/// First-run: offer the LLM wizard so `sync` works out of the box.
fn offer_credentials() -> Result<(), String> {
    let configured =
        tiered_memory::LlmConfig::resolve(None, &data_root()).map_err(|e| e.to_string())?;
    if configured.is_some() {
        return Ok(());
    }
    if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        print!("\nset up an LLM provider now (needed for `sync` memory extraction)? [Y/n] ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if matches!(line.trim(), "n" | "N" | "no" | "No") {
            println!("skipped — run `tiered-memory credentials` anytime");
        } else {
            crate::credentials::credentials_set(&[])?;
        }
    } else {
        println!("hint: run `tiered-memory credentials` once to enable `sync` (LLM extraction)");
    }
    Ok(())
}

/// First-run: offer the /tiered-memory skill for the agent harnesses the user
/// actually runs — asked once, only while nothing is installed anywhere.
fn offer_skill_install(cwd: &Path) -> Result<(), String> {
    let home = home_dir();
    match tiered_memory::harnesses::any_installed(&home, cwd) {
        Some((h, p)) => println!(
            "agent skill already installed for {}: {}",
            h.label,
            p.display()
        ),
        None => {
            if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
                print!("\ninstall the /tiered-memory agent skill for your harness(es)? [Y/n] ");
                std::io::stdout().flush().map_err(|e| e.to_string())?;
                let mut line = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .map_err(|e| e.to_string())?;
                if matches!(line.trim(), "n" | "N" | "no" | "No") {
                    println!("skipped — run `tiered-memory install-skill` anytime");
                } else {
                    crate::skill::install_skill_from_picker(&home, cwd)?;
                }
            } else {
                println!("hint: install the agent skill with `tiered-memory install-skill`");
            }
        }
    }
    Ok(())
}

/// Append `entry` to a `.gitignore`, creating it if needed. Returns false when
/// the entry is already present (idempotent re-inits don't duplicate lines).
fn append_gitignore(path: &Path, entry: &str) -> Result<bool, String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == entry) {
        return Ok(false);
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("# tiered-memory\n");
    out.push_str(entry);
    out.push('\n');
    std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

// -- project detection -------------------------------------------------------

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

pub(crate) fn projects() -> Result<(), String> {
    let args = cmd_args();
    if args.first().map(String::as_str) == Some("remove") {
        return project_remove(&args);
    }
    let (user, _) = flags(&args);
    let db = store()?
        .load(&user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}`"))?;

    if db.projects.is_empty() {
        println!("no projects registered for `{user}` yet — cd into a project and run `tiered-memory init`");
        return Ok(());
    }
    let current = crate::current_selection(&user);
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
        let name = if p.name.is_empty() {
            id.as_str()
        } else {
            p.name.as_str()
        };
        println!(
            "  {id:<24} L1 {l1:>3}  similar: {:?}  {name}{cur}",
            p.similar
        );
    }
    println!("\nselect one with: tiered-memory select --user {user} [--project <id>]");
    println!("remove one with: tiered-memory projects remove <id> [--user {user}]");
    Ok(())
}

/// `projects remove <id>` — unregister a project and forget all of its
/// records. HTTP-first like every write; the store reconciles the project's
/// L1 folder and link entries on save.
fn project_remove(args: &[String]) -> Result<(), String> {
    let user = arg_value(args, "--user").unwrap_or_else(default_user);
    let id = args
        .get(1)
        .filter(|s| !s.starts_with('-'))
        .ok_or("usage: tiered-memory projects remove <id> [--user U]")?;
    match crate::api_delete(&format!("/v1/projects/{user}/{id}")) {
        crate::Api::Ok(v) => println!(
            "removed `{id}` ({} memories forgotten)",
            v.as_u64().unwrap_or(0)
        ),
        crate::Api::Err(e) => return Err(e),
        crate::Api::Unreachable => {
            let n = local_engine()?
                .remove_project(&user, id)
                .map_err(|e| e.to_string())?;
            println!("removed `{id}` ({n} memories forgotten) — local store");
        }
    }
    Ok(())
}

// -- select ------------------------------------------------------------------

pub(crate) fn select() -> Result<(), String> {
    let args = cmd_args();
    let (user, explicit) = flags(&args);
    let db = store()?
        .load(&user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}` — register projects first"))?;

    if let Some(id) = explicit {
        return set_selection(&user, &id);
    }

    let ids: Vec<&String> = db.projects.keys().collect();
    if ids.is_empty() {
        return Err(format!("no projects registered for `{user}` yet"));
    }

    let current = crate::current_selection(&user);
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
    set_selection(&user, &ids[n - 1])
}

fn set_selection(user: &str, id: &str) -> Result<(), String> {
    LayeredDirStore::new(crate::data_root())
        .map_err(|e| e.to_string())?
        .set_current_project(user, id)
        .map_err(|e| e.to_string())?;
    println!("current project for `{user}` → {id}");
    Ok(())
}

// -- group -------------------------------------------------------------------

/// `tiered-memory group` — L2 group management. Bare: show this project's
/// group, with a suggestion derived from similar projects when unset.
/// `group set <name>` assigns; `group set none` records an explicit
/// "belongs to no group" so the /tiered-memory skill never asks again.
/// Assignments are user-authoritative — automatic clustering only proposes.
pub(crate) fn group_cmd() -> Result<(), String> {
    let args = cmd_args();
    let user = arg_value(&args, "--user").unwrap_or_else(default_user);

    if args.first().map(String::as_str) == Some("set") {
        return group_set(&args, &user);
    }
    group_show(&args, &user)
}

fn group_set(args: &[String], user: &str) -> Result<(), String> {
    let name = args
        .get(1)
        .filter(|s| !s.starts_with('-'))
        .ok_or("usage: tiered-memory group set <name|none> [--project P]")?;
    let project = resolve_project(args, user)?;
    let body = serde_json::json!({
        "user": user,
        "project_id": project,
        "group": name,
    });
    service_or_local(
        "/v1/projects/group",
        &body,
        |_| Ok(()),
        || {
            local_engine()?
                .set_project_group(user, &project, Some(name))
                .map_err(|e| e.to_string())
                .map(|_| ())
        },
    )?;

    if name == tiered_memory::NO_GROUP {
        println!("`{project}`: confirmed no L2 group — the /tiered-memory skill won't ask again");
        return Ok(());
    }
    println!("`{project}` → L2 group `{name}`");
    if let Some(db) = store()?.load(user).map_err(|e| e.to_string())? {
        let mates: Vec<String> = db
            .projects
            .iter()
            .filter(|(id, p)| {
                id.as_str() != project.as_str() && p.group.as_deref() == Some(name.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect();
        if mates.is_empty() {
            println!(
                "  (first member — this project's L2 memories now surface to every project later added to `{name}`)"
            );
        } else {
            println!("  group-mates: {}", mates.join(", "));
        }
    }
    Ok(())
}

fn group_show(args: &[String], user: &str) -> Result<(), String> {
    let project = resolve_project(args, user)?;
    let db = store()?
        .load(user)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no memory yet for user `{user}`"))?;
    let Some(p) = db.projects.get(&project) else {
        return Err(format!(
            "project `{project}` is not registered — run `tiered-memory init` in its directory"
        ));
    };
    match p.group.as_deref() {
        Some(g @ tiered_memory::NO_GROUP) => {
            println!("group: {g} (confirmed — the /tiered-memory skill won't ask again)");
        }
        Some(g) => println!("group: {g}"),
        None => {
            println!("group: (unset)");
            print_group_suggestion(&db, p);
        }
    }
    Ok(())
}

/// While a project is unassigned, propose the most common group among its
/// similar projects — the same proposal the agent skill confirms with the user.
fn print_group_suggestion(db: &UserDb, p: &ProjectInfo) {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for sid in &p.similar {
        if let Some(sp) = db.projects.get(sid) {
            if let Some(g) = sp
                .group
                .as_deref()
                .filter(|g| *g != tiered_memory::NO_GROUP)
            {
                *counts.entry(g).or_default() += 1;
            }
        }
    }
    if let Some((g, n)) = counts.into_iter().max_by_key(|(_, n)| *n) {
        println!("suggestion: {g} ({n} similar project(s) already use it)");
    } else if !p.similar.is_empty() {
        println!(
            "similar projects: {} — none grouped yet; ask which family this belongs to, or create one",
            p.similar.join(", ")
        );
    } else {
        println!(
            "no suggestion — ask the user which family of projects this belongs to (e.g. rust-clis, web-apps), or `tiered-memory group set none`"
        );
    }
}
