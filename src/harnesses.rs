//! Agent-harness registry: where the harnesses people actually run look for
//! skills, detection, and the install logic behind `tiered-memory
//! install-skill` / the `init` first-run wizard.
//!
//! The skill itself is one portable file (`SKILL.md`, Agent Skills format) —
//! what differs per harness is only the directory (or, for harnesses without
//! a skills mechanism, an instructions block appended to `AGENTS.md`). The
//! list is best-effort: paths move between harness versions, so every entry
//! shows its target path and any harness can be reached with `--dir`.
#![cfg(feature = "server")]

use crate::error::{MemoryError, Result};
use std::path::{Path, PathBuf};

/// How a harness consumes the skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Copy `SKILL.md` into `<dir>/tiered-memory/`.
    SkillDir,
    /// Append a marked instruction block to an `AGENTS.md`-style file
    /// (harnesses that have no skills mechanism; idempotent, re-install
    /// updates the block between the markers).
    AgentsMd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Harness {
    /// Stable id for `install-skill --harness <id>`.
    pub id: &'static str,
    pub label: &'static str,
    /// User-level skills dir relative to `$HOME` (SkillDir mode), or the
    /// rules/AGENTS.md file relative to the project (AgentsMd mode).
    pub target: &'static str,
    /// Anchor for `target`: the user home or the current project.
    pub project_scoped: bool,
    /// Directories (relative to `$HOME`) whose presence suggests this harness
    /// is installed on the machine.
    pub detect: &'static [&'static str],
    pub mode: InstallMode,
    pub note: &'static str,
    /// Defined by the user in `{data}/harnesses.json`, not built in.
    pub custom: bool,
}

pub const SKILL_FILE: &str = include_str!("../skill/SKILL.md");

pub const AGENTS_BLOCK_START: &str = "<!-- tiered-memory:start -->";
pub const AGENTS_BLOCK_END: &str = "<!-- tiered-memory:end -->";

/// Thin sibling skills that surface tiered-memory's subcommands in the
/// harness's own autocomplete: typing `/tiered-memory` then narrows to
/// `tiered-memory-sync`, `tiered-memory-recall`, … Harnesses have no native
/// subcommand mechanism — one skill, freeform text after it — so each
/// subcommand becomes its own completable entry (opt-in via
/// `install-skill --subcommands`). Each lives in
/// `<skills>/<name>/SKILL.md` next to the main skill.
pub struct SubSkill {
    /// Directory/skill name suffix: `tiered-memory<suffix>`.
    pub suffix: &'static str,
    pub description: &'static str,
    /// Instructions the harness follows when the skill fires.
    pub body: &'static str,
}

pub const SUBCOMMAND_SKILLS: &[SubSkill] = &[
    SubSkill {
        suffix: "-sync",
        description: "Sync this session into long-term memory (L1/L2/L3). Use at session end, or when the user asks to update/remember/save memory.",
        body: "Run `tiered-memory sync --stdin` with the session transcript (add --dry-run to preview). Report one line per stored memory, grouped by layer, then show `tiered-memory params` if parameters changed. If credentials are missing, say `tiered-memory credentials` sets them up.",
    },
    SubSkill {
        suffix: "-recall",
        description: "Search the learner's long-term memory. Use when prior preferences, traits or project context would help.",
        body: "Run `tiered-memory recall \"<the query>\"` (add --k N for more hits) and summarize the hits with their layer ([L1]/[L2]/[L3]).",
    },
    SubSkill {
        suffix: "-remember",
        description: "Store a durable fact or preference about the learner or this project.",
        body: "Run `tiered-memory remember \"<one short third-person sentence>\"`. Route it: project-local → plain (L1); learner-wide trait → `--global` (L3); true of a whole group of projects → `--group <name>` or `--level L2 --topic <slug>`. Add `--param key=value` for anything tunable. Never store secrets.",
    },
    SubSkill {
        suffix: "-group",
        description: "Show or set this project's L2 group (the family of projects it shares warm memories with).",
        body: "Run `tiered-memory group` to show the current group and suggestion. To set: `tiered-memory group set <name|none>` — ask the user which family the project belongs to first; their answer is authoritative and `none` stops future asking.",
    },
    SubSkill {
        suffix: "-params",
        description: "Show the learner's adjusted parameters (difficulty, pace, style…) merged across layers.",
        body: "Run `tiered-memory params` and present the resulting parameter set.",
    },
];

/// Directory name of a sub-skill inside a skills root.
fn sub_skill_dir(suffix: &str) -> String {
    format!("tiered-memory{suffix}")
}

/// The SKILL.md for one sub-skill.
pub fn sub_skill_md(sub: &SubSkill) -> String {
    format!(
        "---\nname: tiered-memory{}\ndescription: {}\n---\n\n# tiered-memory {}\n\n{}\n",
        sub.suffix,
        sub.description,
        sub.suffix.trim_start_matches('-'),
        sub.body
    )
}

/// The built-in install targets. Order matters: detected harnesses float to
/// the top of the picker regardless. Anything missing can be added by the
/// user in `{data}/harnesses.json` — see [`registry`].
pub const KNOWN: &[Harness] = &[
    Harness {
        id: "agents",
        label: "Agent Skills spec (ZCode & friends)",
        target: ".agents/skills",
        project_scoped: false,
        detect: &[".agents", ".zcode"],
        mode: InstallMode::SkillDir,
        note: "the default install target",
        custom: false,
    },
    Harness {
        id: "claude",
        label: "Claude Code (user skills)",
        target: ".claude/skills",
        project_scoped: false,
        detect: &[".claude"],
        mode: InstallMode::SkillDir,
        note: "",
        custom: false,
    },
    Harness {
        id: "claude-project",
        label: "Claude Code (this project)",
        target: ".claude/skills",
        project_scoped: true,
        detect: &[],
        mode: InstallMode::SkillDir,
        note: "only visible inside this project",
        custom: false,
    },
    Harness {
        id: "codex",
        label: "Codex CLI (AGENTS.md)",
        target: "AGENTS.md",
        project_scoped: true,
        detect: &[".codex"],
        mode: InstallMode::AgentsMd,
        note: "instruction block appended to AGENTS.md",
        custom: false,
    },
    Harness {
        id: "opencode",
        label: "OpenCode",
        target: ".config/opencode/skill",
        project_scoped: false,
        detect: &[".config/opencode"],
        mode: InstallMode::SkillDir,
        note: "best effort — verify the path for your version",
        custom: false,
    },
    Harness {
        id: "gemini",
        label: "Gemini CLI",
        target: ".gemini/skills",
        project_scoped: false,
        detect: &[".gemini"],
        mode: InstallMode::SkillDir,
        note: "best effort — verify the path for your version",
        custom: false,
    },
    // Rules-file harnesses: no skills mechanism, but they read project
    // markdown — the marked instruction block works everywhere (AgentsMd).
    Harness {
        id: "junie",
        label: "Junie (JetBrains)",
        target: ".junie/guidelines.md",
        project_scoped: true,
        detect: &[],
        mode: InstallMode::AgentsMd,
        note: "block appended to the project guidelines Junie reads",
        custom: false,
    },
    Harness {
        id: "aider",
        label: "Aider",
        target: "CONVENTIONS.md",
        project_scoped: true,
        detect: &[],
        mode: InstallMode::AgentsMd,
        note: "add to aider with `/read CONVENTIONS.md`",
        custom: false,
    },
    Harness {
        id: "cline",
        label: "Cline / Roo Code",
        target: ".clinerules",
        project_scoped: true,
        detect: &[],
        mode: InstallMode::AgentsMd,
        note: "block appended to the project rules file",
        custom: false,
    },
    Harness {
        id: "windsurf",
        label: "Windsurf",
        target: ".windsurf/rules/tiered-memory.md",
        project_scoped: true,
        detect: &[".codeium"],
        mode: InstallMode::AgentsMd,
        note: "dedicated rules file — best effort",
        custom: false,
    },
];

/// User-defined harnesses live in `{data}/harnesses.json` — the way to add
/// any harness not built in (CommandCode, private wrappers, whatever ships
/// next). Shape (a bare array also works):
///
/// ```json
/// {
///   "harnesses": [
///     {
///       "id": "myagent",
///       "label": "My Agent",
///       "target": ".myagent/skills",
///       "mode": "skill-dir",
///       "scope": "user",
///       "detect": [".myagent"],
///       "note": "optional"
///     }
///   ]
/// }
/// ```
///
/// `mode` is `skill-dir` (copy SKILL.md into `<target>/tiered-memory/`) or
/// `agents-md` (append the marked instruction block to the target file —
/// works for any rules/guidelines markdown). `scope` anchors `target` at
/// `$HOME` (`user`) or the current project (`project`).
#[derive(Debug, serde::Deserialize)]
struct UserHarnessFile {
    #[serde(default)]
    harnesses: Vec<UserHarness>,
}

#[derive(Debug, serde::Deserialize)]
struct UserHarness {
    id: String,
    label: String,
    target: String,
    mode: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    detect: Vec<String>,
    #[serde(default)]
    note: String,
}

/// The full registry: built-ins plus user-defined harnesses. Entries are
/// `'static` because the binary is a short-lived process — user definitions
/// are `Box::leak`ed once per run, which keeps every consumer (picker,
/// installer, clean, console) working over plain `&Harness` with no lifetime
/// plumbing.
pub fn registry(home: &Path) -> Vec<&'static Harness> {
    let data_dir = std::env::var("TM_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::default_data_dir());
    registry_with_data_dir(home, &data_dir)
}

/// [`registry`] with an explicit data dir — the test seam.
pub fn registry_with_data_dir(home: &Path, data_dir: &Path) -> Vec<&'static Harness> {
    let mut all: Vec<&'static Harness> = KNOWN.iter().collect();
    all.extend(user_harnesses(home, data_dir));
    all
}

/// Parse `harnesses.json` from the data dir (if present) and leak the
/// definitions. Invalid entries are reported to stderr and skipped — one bad
/// line should not break the whole registry.
fn user_harnesses(_home: &Path, data_dir: &Path) -> Vec<&'static Harness> {
    let path = data_dir.join("harnesses.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    // accepted shapes: {"harnesses": [...]} or a bare [...]
    let file: UserHarnessFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(first_err) => match serde_json::from_str::<Vec<UserHarness>>(&text) {
            Ok(list) => UserHarnessFile { harnesses: list },
            Err(_) => {
                eprintln!(
                    "tiered-memory: ignoring {} (invalid: {first_err})",
                    path.display()
                );
                return Vec::new();
            }
        },
    };
    let mut out = Vec::new();
    for u in file.harnesses {
        match parse_user_harness(u) {
            Ok(h) => out.push(Box::leak(Box::new(h)) as &'static Harness),
            Err(e) => eprintln!(
                "tiered-memory: ignoring an entry in {}: {e}",
                path.display()
            ),
        }
    }
    out
}

fn parse_user_harness(u: UserHarness) -> std::result::Result<Harness, String> {
    if !crate::store::valid_path_segment(&u.id) {
        return Err(format!("harness id `{}` is not path-safe", u.id));
    }
    if KNOWN.iter().any(|h| h.id == u.id) {
        return Err(format!("harness id `{}` clashes with a built-in", u.id));
    }
    if u.target.trim().is_empty() {
        return Err(format!("harness `{}` needs a non-empty target", u.id));
    }
    let mode = match u.mode.as_str() {
        "skill-dir" => InstallMode::SkillDir,
        "agents-md" => InstallMode::AgentsMd,
        other => {
            return Err(format!(
                "harness `{}`: unknown mode `{other}` (skill-dir | agents-md)",
                u.id
            ))
        }
    };
    let project_scoped = match u.scope.as_str() {
        "" | "user" => false,
        "project" => true,
        other => {
            return Err(format!(
                "harness `{}`: unknown scope `{other}` (user | project)",
                u.id
            ))
        }
    };
    // leak the owned strings into 'static (see registry)
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let detect: Vec<&'static str> = u
        .detect
        .into_iter()
        .map(|d| Box::leak(d.into_boxed_str()) as &'static str)
        .collect();
    Ok(Harness {
        id: leak(u.id),
        label: leak(u.label),
        target: leak(u.target),
        project_scoped,
        detect: Box::leak(detect.into_boxed_slice()),
        mode,
        note: leak(u.note),
        custom: true,
    })
}

impl Harness {
    /// Absolute target for this harness: a skills directory (SkillDir) or the
    /// AGENTS.md path (AgentsMd).
    pub fn target_path(&self, home: &Path, cwd: &Path) -> PathBuf {
        let base = if self.project_scoped { cwd } else { home };
        base.join(self.target)
    }

    /// Where the SKILL.md copy lands (SkillDir mode only).
    pub fn skill_file(&self, home: &Path, cwd: &Path) -> Option<PathBuf> {
        match self.mode {
            InstallMode::SkillDir => Some(
                self.target_path(home, cwd)
                    .join("tiered-memory")
                    .join("SKILL.md"),
            ),
            InstallMode::AgentsMd => None,
        }
    }

    pub fn detected(&self, home: &Path) -> bool {
        self.detect.iter().any(|d| home.join(d).exists())
    }
}

/// Install the skill for one harness. Returns the path that now holds it.
pub fn install(h: &Harness, home: &Path, cwd: &Path) -> Result<PathBuf> {
    install_with(h, home, cwd, false)
}

/// Install with the optional `/tiered-memory-*` subcommand skills (SkillDir
/// harnesses only — rules-file harnesses get the condensed block instead).
pub fn install_with(h: &Harness, home: &Path, cwd: &Path, subcommands: bool) -> Result<PathBuf> {
    match h.mode {
        InstallMode::SkillDir => {
            let path = h.skill_file(home, cwd).expect("SkillDir has a skill file");
            let root = path
                .parent()
                .expect("has parent")
                .parent()
                .expect("skills root")
                .to_path_buf();
            std::fs::create_dir_all(path.parent().expect("has parent")).map_err(|e| {
                MemoryError::Storage(format!("create {}: {e}", path.parent().unwrap().display()))
            })?;
            std::fs::write(&path, SKILL_FILE)
                .map_err(|e| MemoryError::Storage(format!("write {}: {e}", path.display())))?;
            if subcommands {
                for sub in SUBCOMMAND_SKILLS {
                    let dir = root.join(sub_skill_dir(sub.suffix));
                    std::fs::create_dir_all(&dir).map_err(|e| {
                        MemoryError::Storage(format!("create {}: {e}", dir.display()))
                    })?;
                    std::fs::write(dir.join("SKILL.md"), sub_skill_md(sub)).map_err(|e| {
                        MemoryError::Storage(format!(
                            "write {}: {e}",
                            dir.join("SKILL.md").display()
                        ))
                    })?;
                }
            }
            Ok(path)
        }
        InstallMode::AgentsMd => {
            let path = h.target_path(home, cwd);
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let block = format!(
                "{AGENTS_BLOCK_START}\n{}\n{AGENTS_BLOCK_END}",
                agents_block_body().trim_end()
            );
            let updated = match (
                existing.find(AGENTS_BLOCK_START),
                existing.find(AGENTS_BLOCK_END),
            ) {
                (Some(s), Some(e)) if e > s => {
                    // replace the block between (and including) the markers
                    let mut out = String::with_capacity(existing.len() + block.len());
                    out.push_str(&existing[..s]);
                    out.push_str(&block);
                    out.push_str(existing[e + AGENTS_BLOCK_END.len()..].trim_start_matches('\n'));
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out
                }
                _ => {
                    let mut out = existing;
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&block);
                    out
                }
            };
            std::fs::write(&path, updated)
                .map_err(|e| MemoryError::Storage(format!("write {}: {e}", path.display())))?;
            Ok(path)
        }
    }
}

/// Remove the skill for one harness. Returns the path that was cleaned, or
/// `None` when nothing was installed. SkillDir: deletes the
/// `tiered-memory/` skills directory. AgentsMd: strips the marked block from
/// the file — deleting the file entirely when nothing else remains.
pub fn uninstall(h: &Harness, home: &Path, cwd: &Path) -> Result<Option<PathBuf>> {
    match h.mode {
        InstallMode::SkillDir => {
            let Some(skill_path) = installed_path(h, home, cwd) else {
                return Ok(None);
            };
            let dir = skill_path
                .parent()
                .expect("SKILL.md always has a parent directory");
            std::fs::remove_dir_all(dir)
                .map_err(|e| MemoryError::Storage(format!("remove {}: {e}", dir.display())))?;
            // sub-command skills are siblings of the main dir (opt-in installs)
            let root = dir
                .parent()
                .expect("skill dir always has a skills root")
                .to_path_buf();
            for sub in SUBCOMMAND_SKILLS {
                let sub_dir = root.join(sub_skill_dir(sub.suffix));
                if sub_dir.is_dir() {
                    std::fs::remove_dir_all(&sub_dir).map_err(|e| {
                        MemoryError::Storage(format!("remove {}: {e}", sub_dir.display()))
                    })?;
                }
            }
            Ok(Some(dir.to_path_buf()))
        }
        InstallMode::AgentsMd => {
            let path = h.target_path(home, cwd);
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(MemoryError::Storage(format!(
                        "read {}: {e}",
                        path.display()
                    )))
                }
            };
            let (Some(s), Some(e)) = (text.find(AGENTS_BLOCK_START), text.find(AGENTS_BLOCK_END))
            else {
                return Ok(None);
            };
            if e <= s {
                return Ok(None);
            }
            let mut out = String::with_capacity(text.len());
            out.push_str(text[..s].trim_end());
            let rest = text[e + AGENTS_BLOCK_END.len()..].trim_start_matches('\n');
            if !rest.is_empty() {
                if !out.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(rest);
            }
            if out.trim().is_empty() {
                std::fs::remove_file(&path)
                    .map_err(|e| MemoryError::Storage(format!("remove {}: {e}", path.display())))?;
            } else {
                std::fs::write(&path, format!("{out}\n"))
                    .map_err(|e| MemoryError::Storage(format!("write {}: {e}", path.display())))?;
            }
            Ok(Some(path))
        }
    }
}

/// The instruction block for AGENTS.md-style harnesses — a condensed protocol
/// (the full SKILL.md is too heavy to inline everywhere).
fn agents_block_body() -> String {
    format!(
        "# tiered-memory (agent skill)\n\n\
         Long-term learner memory: L1 = this project, L2 = related projects/groups, \
         L3 = global traits. Binary: `tiered-memory`.\n\n\
         At session start run `tiered-memory params`, `tiered-memory recall \"<topic>\"` and \
         `tiered-memory group`; shape the session from what comes back. Store durable \
         signals as they happen (`tiered-memory remember \"…\" [--param k=v]`, \
         `tiered-memory feedback <key> <value>`, `--global` for cross-project traits). \
         Never store secrets. At session end run `tiered-memory sync --stdin` with the \
         transcript if LLM credentials are configured. Full protocol: \
         `tiered-memory install-skill --harness agents` then read the installed SKILL.md.\n\
         Data: {} (delete it to forget everything).",
        crate::default_data_dir().display()
    )
}

/// Was the skill already installed for this harness?
pub fn installed_path(h: &Harness, home: &Path, cwd: &Path) -> Option<PathBuf> {
    match h.mode {
        InstallMode::SkillDir => {
            let p = h.skill_file(home, cwd)?;
            p.is_file().then_some(p)
        }
        InstallMode::AgentsMd => {
            let p = h.target_path(home, cwd);
            let text = std::fs::read_to_string(&p).ok()?;
            let has = text.contains(AGENTS_BLOCK_START) && text.contains(AGENTS_BLOCK_END);
            has.then_some(p)
        }
    }
}

/// Any harness with the skill installed (used by the `init` wizard to decide
/// whether to offer installation).
pub fn any_installed(home: &Path, cwd: &Path) -> Option<(&'static Harness, PathBuf)> {
    registry(home)
        .into_iter()
        .find_map(|h| installed_path(h, home, cwd).map(|p| (h, p)))
}

/// One installed (or missing) skill copy — what the console's Skills panel
/// and `install-skill --list` show.
#[derive(Debug, Clone)]
pub struct SkillInstall {
    pub harness: &'static str,
    pub label: String,
    pub path: PathBuf,
    pub installed: bool,
    /// One-line summary parsed from the SKILL.md frontmatter.
    pub description: String,
}

/// Scan every known harness and report install status + the skill's own
/// description (so the console can show what each copy is without opening it).
pub fn summaries(home: &Path, cwd: &Path) -> Vec<SkillInstall> {
    registry(home)
        .iter()
        .map(|h| {
            let path = installed_path(h, home, cwd);
            let description = path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .as_deref()
                .and_then(parse_description)
                .unwrap_or_default();
            SkillInstall {
                harness: h.id,
                label: h.label.to_string(),
                path: path.clone().unwrap_or_else(|| h.target_path(home, cwd)),
                installed: path.is_some(),
                description,
            }
        })
        .collect()
}

/// First `description:` line of the SKILL.md YAML frontmatter.
fn parse_description(skill_md: &str) -> Option<String> {
    let mut in_frontmatter = false;
    for line in skill_md.lines() {
        let line = line.trim();
        if line == "---" {
            if in_frontmatter {
                break;
            }
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if let Some(rest) = line.strip_prefix("description:") {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

// -- interactive picker ------------------------------------------------------

/// Multi-select harness picker (↑/↓ move, space toggles, `a` toggles all,
/// Enter confirms, Esc cancels → `None`). Detected harnesses are listed and
/// pre-highlighted first.
pub fn pick_harnesses(home: &Path, cwd: &Path) -> Result<Option<Vec<&'static Harness>>> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::style::Stylize;
    use std::io::{stdout, Write};

    // detected first, then the rest; project-scoped last
    let mut items: Vec<&'static Harness> = registry(home);
    items.sort_by_key(|h| (!h.detected(home), h.project_scoped));

    let mut selected = vec![false; items.len()];
    for (i, h) in items.iter().enumerate() {
        selected[i] = h.detected(home) && installed_path(h, home, cwd).is_none();
    }
    let mut cursor = 0usize;

    let _guard = crate::tui::RawGuard::enter()?;
    let draw = |cursor: usize, selected: &[bool], items: &[&Harness]| -> std::io::Result<()> {
        let mut out = stdout();
        crossterm::execute!(
            out,
            crossterm::cursor::MoveTo(0, 0),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        writeln!(
            out,
            "{}",
            "Install the /tiered-memory skill for which harnesses?".bold()
        )?;
        writeln!(
            out,
            "{}",
            "(space: toggle · a: all · enter: install · esc: cancel)\n".dim()
        )?;
        for (i, h) in items.iter().enumerate() {
            let mark = if selected[i] { "[x]" } else { "[ ]" };
            let live = if h.detected(home) { "● detected" } else { "" };
            let done = if installed_path(h, home, cwd).is_some() {
                "· already installed"
            } else {
                ""
            };
            let cursor_mark = if i == cursor { ">" } else { " " };
            writeln!(
                out,
                "{cursor_mark} {mark} {:<38} {:<24} {} {}",
                h.label,
                h.target,
                live.dim(),
                done.dim()
            )?;
        }
        writeln!(
            out,
            "\n{}",
            "other directory: tiered-memory install-skill --dir <path>".dim()
        )?;
        out.flush()
    };

    loop {
        draw(cursor, &selected, &items)
            .map_err(|e| MemoryError::invalid(format!("terminal: {e}")))?;
        if !event::poll(std::time::Duration::from_millis(250))
            .map_err(|e| MemoryError::invalid(format!("terminal: {e}")))?
        {
            continue;
        }
        let Event::Key(k) =
            event::read().map_err(|e| MemoryError::invalid(format!("terminal: {e}")))?
        else {
            continue;
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Down => cursor = (cursor + 1).min(items.len() - 1),
            KeyCode::Up => cursor = cursor.saturating_sub(1),
            KeyCode::Char(' ') => selected[cursor] = !selected[cursor],
            KeyCode::Char('a') => {
                let all = selected.iter().all(|s| *s);
                selected.iter_mut().for_each(|s| *s = !all);
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                let chosen: Vec<&Harness> = items
                    .iter()
                    .zip(&selected)
                    .filter(|(_, s)| **s)
                    .map(|(h, _)| *h)
                    .collect();
                if chosen.is_empty() {
                    continue;
                }
                return Ok(Some(chosen));
            }
            _ => {}
        }
    }
}

// -- hooks -------------------------------------------------------------------
// Hooks make memory automatic: the harness fires a command at session start
// (tiered-memory injects the learner brief into context) and at session end
// (tiered-memory syncs the transcript into the layers) — no skill invocation
// needed. Support is per harness, because every tool configures hooks
// differently.

/// Which hook configuration format a harness speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// Claude Code: `~/.claude/settings.json` → `hooks.SessionStart|SessionEnd`.
    /// SessionStart stdout is added to context; SessionEnd receives the
    /// transcript path on stdin.
    Claude,
    /// ZCode: `~/.zcode/cli/config.json` → `hooks.events.SessionStart`.
    /// Config-file hooks must set `hooks.enabled: true`, and there is **no
    /// SessionEnd event** — end-of-session capture stays skill-driven there.
    Zcode,
}

/// The harnesses that can hook into memory, with their settings file.
pub fn hook_support(h: &Harness, home: &Path) -> Option<(HookKind, PathBuf)> {
    match h.id {
        "claude" | "claude-project" => {
            Some((HookKind::Claude, home.join(".claude").join("settings.json")))
        }
        "agents" => Some((
            HookKind::Zcode,
            home.join(".zcode").join("cli").join("config.json"),
        )),
        _ => None,
    }
}

/// Install hooks for one harness: merge the SessionStart (and SessionEnd,
/// where the harness has one) commands into its settings JSON, replacing any
/// previous tiered-memory entries so re-runs are idempotent. Returns the
/// settings file written.
pub fn install_hooks(h: &Harness, home: &Path, _cwd: &Path) -> Result<Option<PathBuf>> {
    let Some((kind, path)) = hook_support(h, home) else {
        return Ok(None);
    };
    let mut root = read_json_or_empty(&path)?;
    let events_path: &[&str] = match kind {
        HookKind::Claude => &["hooks"],
        // ZCode nests events one level deeper and needs the runner enabled
        HookKind::Zcode => &["hooks", "events"],
    };
    ensure_object(&mut root, events_path)?;
    if let HookKind::Zcode = kind {
        root["hooks"]["enabled"] = serde_json::json!(true);
    }

    let mut events: Vec<(&str, &str)> = vec![("SessionStart", "startup|resume")];
    if let HookKind::Claude = kind {
        events.push(("SessionEnd", ""));
    }
    for (event, matcher) in events {
        let command = match (kind, event) {
            // ZCode parses stdout as strict JSON — emit additionalContext
            (HookKind::Zcode, "SessionStart") => "tiered-memory hook session-start --format json",
            (_, "SessionStart") => "tiered-memory hook session-start",
            (_, "SessionEnd") => "tiered-memory hook session-end",
            _ => continue,
        };
        let entry = json_hook_entry(matcher, command);
        replace_event(&mut root, events_path, event, entry);
    }

    write_json(&path, &root)?;
    Ok(Some(path))
}

/// Remove tiered-memory hooks from one harness's settings. Returns the file
/// touched, or `None` when there was nothing of ours to remove.
pub fn remove_hooks(h: &Harness, home: &Path, _cwd: &Path) -> Result<Option<PathBuf>> {
    let Some((kind, path)) = hook_support(h, home) else {
        return Ok(None);
    };
    let mut root = read_json_or_empty(&path)?;
    let events_path: &[&str] = match kind {
        HookKind::Claude => &["hooks"],
        HookKind::Zcode => &["hooks", "events"],
    };
    let mut touched = false;
    for event in ["SessionStart", "SessionEnd"] {
        if strip_event(&mut root, events_path, event) {
            touched = true;
        }
    }
    if !touched {
        return Ok(None);
    }
    write_json(&path, &root)?;
    Ok(Some(path))
}

// -- hooks JSON helpers (serde_json::Value surgery, no schema assumptions) —

fn read_json_or_empty(path: &Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| MemoryError::Storage(format!("corrupt {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(MemoryError::Storage(format!(
            "read {}: {e}",
            path.display()
        ))),
    }
}

fn write_json(path: &Path, root: &serde_json::Value) -> Result<()> {
    let text = serde_json::to_string_pretty(root)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| MemoryError::Storage(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| MemoryError::Storage(format!("write {}: {e}", path.display())))?;
    Ok(())
}

fn ensure_object(root: &mut serde_json::Value, path: &[&str]) -> Result<()> {
    let mut cur = root;
    for key in path {
        if !cur.is_object() {
            *cur = serde_json::json!({});
        }
        cur = cur
            .as_object_mut()
            .expect("just ensured object")
            .entry(key.to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    Ok(())
}

fn json_hook_entry(matcher: &str, command: &str) -> serde_json::Value {
    let hook = serde_json::json!({ "type": "command", "command": command });
    if matcher.is_empty() {
        serde_json::json!({ "hooks": [hook] })
    } else {
        serde_json::json!({ "matcher": matcher, "hooks": [hook] })
    }
}

/// Drop any existing tiered-memory entries under `event`, then append ours.
fn replace_event(
    root: &mut serde_json::Value,
    events_path: &[&str],
    event: &str,
    entry: serde_json::Value,
) {
    let mut cur = root;
    for key in events_path {
        cur = &mut cur[*key];
    }
    let list = cur.as_object_mut().map(|o| {
        o.entry(event.to_string())
            .or_insert_with(|| serde_json::json!([]))
    });
    let Some(list) = list else { return };
    if let Some(items) = list.as_array_mut() {
        items.retain(|item| !mentions_tiered_memory(item));
        items.push(entry);
    }
}

/// Drop tiered-memory entries under `event`; true when something was removed.
fn strip_event(root: &mut serde_json::Value, events_path: &[&str], event: &str) -> bool {
    let mut cur = root;
    for key in events_path {
        let Some(next) = cur.get_mut(*key) else {
            return false;
        };
        cur = next;
    }
    let Some(items) = cur.get_mut(event).and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let before = items.len();
    items.retain(|item| !mentions_tiered_memory(item));
    before != items.len()
}

fn mentions_tiered_memory(item: &serde_json::Value) -> bool {
    item.get("hooks")
        .and_then(|h| h.as_array())
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains("tiered-memory hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
