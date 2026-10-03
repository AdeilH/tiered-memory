//! Agent-facing extras: `install-skill` (harness-aware skill installer) and
//! `console` (the read-only terminal dashboard).

use std::path::{Path, PathBuf};

use crate::{arg_switch, arg_value, cmd_args, flags, home_dir};
use tiered_memory::harnesses::{self, Harness, SKILL_FILE};

// -- install-skill -----------------------------------------------------------

/// Install the bundled `/tiered-memory` agent skill. Interactive (default):
/// a multi-select picker over known harnesses, detected ones first. Flags:
/// `--harness <id,id>` for scripts, `--dir D` for a custom directory,
/// `--subcommands` to also install the completable `/tiered-memory-*`
/// sub-skills, `--list` to show the registry.
pub(crate) fn install_skill() -> Result<(), String> {
    let args = cmd_args();
    let home = home_dir();
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let subcommands = arg_switch(&args, "--subcommands");

    if arg_switch(&args, "--list") {
        return list_harnesses(&home, &cwd);
    }
    if let Some(spec) = arg_value(&args, "--harness") {
        let mut installed = Vec::new();
        for h in parse_harness_ids(&spec)? {
            let path =
                harnesses::install_with(h, &home, &cwd, subcommands).map_err(|e| e.to_string())?;
            installed.push((h.label.to_string(), path));
        }
        print_installed(&installed);
        return Ok(());
    }
    if let Some(dir) = arg_value(&args, "--dir") {
        let path = install_into_custom_dir(&PathBuf::from(dir), subcommands)?;
        print_installed(&[("custom directory".to_string(), path)]);
        return Ok(());
    }
    if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        return install_skill_from_picker(&home, &cwd);
    }
    Err("no TTY — pass --harness <id,id> or --dir <path> (see --list)".into())
}

fn list_harnesses(home: &Path, cwd: &Path) -> Result<(), String> {
    println!("known harnesses (skill: /tiered-memory):\n");
    for h in harnesses::registry(home) {
        let status = match harnesses::installed_path(h, home, cwd) {
            Some(p) => format!("installed → {}", p.display()),
            None => "not installed".to_string(),
        };
        let live = if h.detected(home) { " [detected]" } else { "" };
        let origin = if h.custom { " [custom]" } else { "" };
        println!(
            "  {:<15} {:<42} {}{}{}",
            h.id, h.label, status, live, origin
        );
        if !h.note.is_empty() {
            println!("  {:<15} {}", "", h.note);
        }
    }
    println!(
        "\nadd any harness: edit {} (see README) — or install into a plain directory with --dir",
        crate::data_root().join("harnesses.json").display()
    );
    println!("install: tiered-memory install-skill [--harness <id,id>] [--dir <path>]");
    Ok(())
}

/// `--harness claude,agents` → the matching registry entries.
fn parse_harness_ids(spec: &str) -> Result<Vec<&'static Harness>, String> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|id| {
            harnesses::registry(&crate::home_dir())
                .iter()
                .copied()
                .find(|h| h.id == id)
                .ok_or_else(|| {
                    format!("unknown harness `{id}` — see `tiered-memory install-skill --list`")
                })
        })
        .collect()
}

fn install_into_custom_dir(dir: &Path, subcommands: bool) -> Result<PathBuf, String> {
    let target = dir.join("tiered-memory");
    std::fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
    let path = target.join("SKILL.md");
    std::fs::write(&path, SKILL_FILE).map_err(|e| format!("write {}: {e}", path.display()))?;
    if subcommands {
        for sub in tiered_memory::harnesses::SUBCOMMAND_SKILLS {
            let sub_dir = dir.join(format!("tiered-memory{}", sub.suffix));
            std::fs::create_dir_all(&sub_dir)
                .map_err(|e| format!("create {}: {e}", sub_dir.display()))?;
            std::fs::write(sub_dir.join("SKILL.md"), harnesses::sub_skill_md(sub))
                .map_err(|e| format!("write {}: {e}", sub_dir.join("SKILL.md").display()))?;
        }
    }
    Ok(path)
}

fn print_installed(installed: &[(String, PathBuf)]) {
    println!("skill installed:");
    for (label, path) in installed {
        println!("  {label} → {}", path.display());
    }
    println!("restart/refresh the harness if it caches its skill list");
}

/// The interactive picker + install loop, shared with the `init` first-run
/// wizard. Cancelling is not an error — it just skips.
pub(crate) fn install_skill_from_picker(home: &Path, cwd: &Path) -> Result<(), String> {
    let Some(chosen) = harnesses::pick_harnesses(home, cwd).map_err(|e| e.to_string())? else {
        println!("skipped — run `tiered-memory install-skill` anytime");
        return Ok(());
    };
    // subcommand skills give `/tiered-memory-<tab>` completion in the harness
    let subcommands = if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        print!("also install /tiered-memory-* subcommand skills (sync, recall, …)? [y/N] ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    } else {
        false
    };
    let mut installed = Vec::new();
    for h in chosen {
        let path = harnesses::install_with(h, home, cwd, subcommands).map_err(|e| e.to_string())?;
        installed.push((h.label.to_string(), path));
    }
    print_installed(&installed);
    Ok(())
}

// -- console -----------------------------------------------------------------

/// `tiered-memory console` — the read-only terminal dashboard: layer gauges,
/// projects, L2 groups with their per-topic docs, installed skill copies, and
/// a project↔group graph. `--user U` selects the user (default `local`).
pub(crate) fn console_cmd() -> Result<(), String> {
    let args = cmd_args();
    if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        return Err("console needs an interactive terminal".into());
    }
    let (user, _) = flags(&args);
    tiered_memory::console::run(&user).map_err(|e| e.to_string())
}
