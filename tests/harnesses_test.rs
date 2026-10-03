//! Harness registry: install targets, SKILL.md copies, AGENTS.md blocks.

use tiered_memory::harnesses::{self, InstallMode, KNOWN};

fn temp() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (dir, home, cwd)
}

#[test]
fn registry_ids_are_unique_and_modes_have_targets() {
    let mut ids: Vec<_> = KNOWN.iter().map(|h| h.id).collect();
    ids.sort();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "harness ids must be unique");
    for h in KNOWN {
        assert!(!h.target.is_empty());
        assert!(!h.label.is_empty());
        assert!(
            matches!(h.mode, InstallMode::SkillDir | InstallMode::AgentsMd),
            "{}: unknown mode",
            h.id
        );
    }
}

#[test]
fn skill_dir_install_is_detectable_and_described() {
    let (_d, home, cwd) = temp();
    let known: &'static [harnesses::Harness] = KNOWN;
    let claude = known.iter().find(|h| h.id == "claude").unwrap();

    assert!(harnesses::installed_path(claude, &home, &cwd).is_none());
    let path = harnesses::install(claude, &home, &cwd).unwrap();
    assert_eq!(path, home.join(".claude/skills/tiered-memory/SKILL.md"));
    assert!(path.is_file());
    assert!(harnesses::installed_path(claude, &home, &cwd).is_some());

    // re-install overwrites, doesn't duplicate
    harnesses::install(claude, &home, &cwd).unwrap();

    let summaries = harnesses::summaries(&home, &cwd);
    let mine = summaries.iter().find(|s| s.harness == "claude").unwrap();
    assert!(mine.installed);
    assert!(
        mine.description.contains("L1/L2/L3"),
        "description parsed from the SKILL.md frontmatter: {:?}",
        mine.description
    );
    let others = summaries.iter().filter(|s| s.harness != "claude").count();
    assert_eq!(others, KNOWN.len() - 1);
}

#[test]
fn agents_md_block_is_idempotent_and_replaces_in_place() {
    let (_d, home, cwd) = temp();
    let known: &'static [harnesses::Harness] = KNOWN;
    let codex = known.iter().find(|h| h.id == "codex").unwrap();
    let agents_md = cwd.join("AGENTS.md");

    std::fs::write(&agents_md, "# My project\n\nKeep answers short.\n").unwrap();
    let path = harnesses::install(codex, &home, &cwd).unwrap();
    assert_eq!(path, agents_md);
    let first = std::fs::read_to_string(&agents_md).unwrap();
    assert!(
        first.starts_with("# My project"),
        "existing content preserved"
    );
    assert!(first.contains("Keep answers short."));
    assert!(first.contains("tiered-memory:start"));

    // re-install replaces the block, never duplicates it
    harnesses::install(codex, &home, &cwd).unwrap();
    let second = std::fs::read_to_string(&agents_md).unwrap();
    assert_eq!(
        first.matches("tiered-memory:start").count(),
        1,
        "exactly one block after first install"
    );
    assert_eq!(
        second.matches("tiered-memory:start").count(),
        1,
        "re-install replaces in place"
    );
    assert!(second.contains("Keep answers short."));
}

#[test]
fn uninstall_removes_skill_dirs_and_strips_agents_blocks() {
    let (_d, home, cwd) = temp();
    let known: &'static [harnesses::Harness] = KNOWN;
    let claude = known.iter().find(|h| h.id == "claude").unwrap();
    let codex = known.iter().find(|h| h.id == "codex").unwrap();
    let agents_md = cwd.join("AGENTS.md");
    std::fs::write(&agents_md, "# My project\n\nKeep answers short.\n").unwrap();

    harnesses::install(claude, &home, &cwd).unwrap();
    harnesses::install(codex, &home, &cwd).unwrap();
    assert!(harnesses::installed_path(claude, &home, &cwd).is_some());
    assert!(harnesses::installed_path(codex, &home, &cwd).is_some());

    // skill dir: the whole tiered-memory dir goes
    let removed = harnesses::uninstall(claude, &home, &cwd).unwrap();
    assert_eq!(
        removed,
        Some(home.join(".claude/skills/tiered-memory")),
        "returns the removed path"
    );
    assert!(!home.join(".claude/skills/tiered-memory").exists());
    assert!(harnesses::installed_path(claude, &home, &cwd).is_none());

    // AGENTS.md: block stripped, user content kept
    let removed = harnesses::uninstall(codex, &home, &cwd).unwrap();
    assert_eq!(removed, Some(agents_md.clone()));
    let text = std::fs::read_to_string(&agents_md).unwrap();
    assert!(
        !text.contains("tiered-memory:start"),
        "block markers gone: {text:?}"
    );
    assert!(
        text.contains("# My project") && text.contains("Keep answers short."),
        "user content preserved: {text:?}"
    );
    assert!(harnesses::installed_path(codex, &home, &cwd).is_none());

    // uninstalling again is a no-op
    assert_eq!(harnesses::uninstall(claude, &home, &cwd).unwrap(), None);
    assert_eq!(harnesses::uninstall(codex, &home, &cwd).unwrap(), None);

    // an AGENTS.md that held ONLY our block is removed with the block
    let agents_only = tempfile::tempdir().unwrap();
    let cwd2 = agents_only.path().join("proj");
    std::fs::create_dir_all(&cwd2).unwrap();
    harnesses::install(codex, &home, &cwd2).unwrap();
    assert!(cwd2.join("AGENTS.md").is_file());
    harnesses::uninstall(codex, &home, &cwd2).unwrap();
    assert!(
        !cwd2.join("AGENTS.md").exists(),
        "block-only file is deleted"
    );
}

#[test]
fn subcommand_skills_install_as_siblings_and_uninstall_together() {
    let (_d, home, cwd) = temp();
    let known: &'static [harnesses::Harness] = KNOWN;
    let agents = known.iter().find(|h| h.id == "agents").unwrap();
    let root = home.join(".agents/skills");

    // plain install: only the main skill
    harnesses::install(agents, &home, &cwd).unwrap();
    assert!(root.join("tiered-memory/SKILL.md").is_file());
    assert!(!root.join("tiered-memory-sync").exists());

    // --subcommands: one completable sibling per subcommand
    harnesses::install_with(agents, &home, &cwd, true).unwrap();
    for sub in harnesses::SUBCOMMAND_SKILLS {
        let dir = root.join(format!("tiered-memory{}", sub.suffix));
        let md = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
        assert!(
            md.starts_with("---\nname: tiered-memory"),
            "{dir:?} is a proper skill: {md:?}"
        );
    }
    let sync = std::fs::read_to_string(root.join("tiered-memory-sync/SKILL.md")).unwrap();
    assert!(sync.contains("description: Sync this session"));

    // uninstall removes the whole family
    harnesses::uninstall(agents, &home, &cwd).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("tiered-memory"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no tiered-memory* left: {leftovers:?}"
    );

    // rules-file harnesses never get sub-skills (the block is the condensed form)
    let codex = known.iter().find(|h| h.id == "codex").unwrap();
    harnesses::install_with(codex, &home, &cwd, true).unwrap();
    assert!(!root.join("tiered-memory-sync").exists());
}

#[test]
fn detection_uses_home_directories_and_project_scoping() {
    let (_d, home, cwd) = temp();
    let known: &'static [harnesses::Harness] = KNOWN;
    let claude = known.iter().find(|h| h.id == "claude").unwrap();
    let claude_project = known.iter().find(|h| h.id == "claude-project").unwrap();

    assert!(!claude.detected(&home));
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    assert!(claude.detected(&home));
    // project-scoped targets live under cwd, not home
    assert!(claude_project
        .skill_file(&home, &cwd)
        .unwrap()
        .starts_with(&cwd));
    assert!(claude.skill_file(&home, &cwd).unwrap().starts_with(&home));
}
