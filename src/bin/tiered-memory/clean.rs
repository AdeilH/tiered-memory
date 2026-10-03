//! `tiered-memory clean` — remove everything the tool created on this
//! machine: the data dir (`~/tiered-memory` — all memories **and** the LLM
//! credentials inside it) and every installed agent skill copy, across all
//! known harnesses (including `AGENTS.md` blocks). The binary itself is
//! cargo's to remove.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::{arg_switch, cmd_args, data_root, home_dir};
use tiered_memory::harnesses::{self, Harness};

/// What `clean` found on this machine.
struct CleanPlan {
    data_dir: Option<PathBuf>,
    /// (harness, installed skill path)
    installs: Vec<(&'static Harness, PathBuf)>,
}

impl CleanPlan {
    fn scan(home: &std::path::Path, cwd: &std::path::Path) -> Self {
        let installs = harnesses::registry(home)
            .into_iter()
            .filter_map(|h| harnesses::installed_path(h, home, cwd).map(|p| (h, p)))
            .collect();
        let root = data_root();
        let data_dir = root.is_dir().then_some(root);
        CleanPlan { data_dir, installs }
    }

    fn is_empty(&self) -> bool {
        self.data_dir.is_none() && self.installs.is_empty()
    }

    fn print(&self) {
        if let Some(dir) = &self.data_dir {
            println!("  data dir   {}", dir.display());
            println!("             (all memories, parameters and LLM credentials)");
        }
        for (h, path) in &self.installs {
            println!("  skill      {} — {}", path.display(), h.label);
        }
    }
}

/// `tiered-memory clean [--skills | --data] [--yes]` — interactive by
/// default (it deletes memories and credentials); scripts pass `--yes`.
pub(crate) fn clean_cmd() -> Result<(), String> {
    let args = cmd_args();
    let home = home_dir();
    let cwd = std::env::current_dir().map_err(|e| format!("cannot read cwd: {e}"))?;
    let scope_data = !arg_switch(&args, "--skills");
    let scope_skills = !arg_switch(&args, "--data");
    let assume_yes = arg_switch(&args, "--yes");

    let mut plan = CleanPlan::scan(&home, &cwd);
    if !scope_data {
        plan.data_dir = None;
    }
    if !scope_skills {
        plan.installs.clear();
    }
    if plan.is_empty() {
        println!("nothing to clean — no data dir and no skill installs found");
        return Ok(());
    }
    println!("tiered-memory will remove:");
    plan.print();

    if !assume_yes {
        if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
            println!("\nnot a terminal — re-run with --yes to proceed");
            return Ok(());
        }
        print!("\nremove all of the above? This cannot be undone. [y/N] ");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("clean cancelled — nothing was removed");
            return Ok(());
        }
    }

    for (h, _) in &plan.installs {
        match harnesses::uninstall(h, &home, &cwd) {
            Ok(Some(path)) => println!("removed skill: {} ({})", path.display(), h.label),
            Ok(None) => {}
            Err(e) => eprintln!("tiered-memory: could not remove skill for {}: {e}", h.label),
        }
    }
    if let Some(dir) = &plan.data_dir {
        std::fs::remove_dir_all(dir).map_err(|e| format!("remove {}: {e}", dir.display()))?;
        println!("removed data dir: {}", dir.display());
    }

    println!("\nremaining (managed elsewhere):");
    println!("  binary            → cargo uninstall tiered-memory");
    println!("  project markers   → tiered-memory.json files, remove per project if desired");
    Ok(())
}
