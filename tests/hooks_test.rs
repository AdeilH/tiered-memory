//! User-defined harnesses and hook installation.

use tiered_memory::harnesses::{self, HookKind};

fn temp() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("proj");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    (dir, home, cwd)
}

#[test]
fn user_harnesses_extend_the_registry() {
    let (_d, home, _cwd) = temp();
    let data = _d.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("harnesses.json"),
        r#"{
          "harnesses": [
            {
              "id": "commandcode",
              "label": "CommandCode",
              "target": ".commandcode/skills",
              "mode": "skill-dir",
              "scope": "user",
              "detect": [".commandcode"],
              "note": "defined by the user"
            }
          ]
        }"#,
    )
    .unwrap();

    let reg = harnesses::registry_with_data_dir(&home, &data);
    let mine = reg
        .iter()
        .find(|h| h.id == "commandcode")
        .expect("custom entry");
    assert!(mine.custom);
    assert_eq!(mine.label, "CommandCode");
    assert_eq!(mine.target, ".commandcode/skills");
    assert!(!mine.project_scoped);
    assert!(mine.detected(&home) == false);

    // built-ins still present alongside
    assert!(reg.iter().any(|h| h.id == "agents"));

    // a bare array works too
    std::fs::write(
        data.join("harnesses.json"),
        r#"[{"id": "other", "label": "Other", "target": "RULES.md", "mode": "agents-md", "scope": "project"}]"#,
    )
    .unwrap();
    let reg = harnesses::registry_with_data_dir(&home, &data);
    let other = reg.iter().find(|h| h.id == "other").unwrap();
    assert!(other.project_scoped);
    assert_eq!(
        other.target_path(&home, _d.path().join("proj").as_path()),
        _d.path().join("proj").join("RULES.md")
    );

    // invalid entries are skipped, valid ones kept
    std::fs::write(
        data.join("harnesses.json"),
        r#"{"harnesses": [
            {"id": "agents", "label": "clash", "target": "x", "mode": "skill-dir"},
            {"id": "ok", "label": "Ok", "target": "x", "mode": "skill-dir"}
        ]}"#,
    )
    .unwrap();
    let reg = harnesses::registry_with_data_dir(&home, &data);
    assert!(reg.iter().all(|h| h.id != "agents" || !h.custom));
    assert!(reg.iter().any(|h| h.id == "ok"));
}

#[test]
fn claude_hooks_install_remove_and_preserve_user_settings() {
    let (_d, home, _cwd) = temp();
    let known: &'static [harnesses::Harness] = harnesses::KNOWN;
    let claude = known.iter().find(|h| h.id == "claude").unwrap();
    let settings = home.join(".claude/settings.json");

    // no hooks support entry when the settings don't exist yet — created on install
    let path = harnesses::install_hooks(claude, &home, &_cwd)
        .unwrap()
        .unwrap();
    assert_eq!(path, settings);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    let start = &v["hooks"]["SessionStart"];
    assert_eq!(start[0]["matcher"], "startup|resume");
    assert_eq!(
        start[0]["hooks"][0]["command"],
        "tiered-memory hook session-start"
    );
    let end = &v["hooks"]["SessionEnd"];
    assert_eq!(
        end[0]["hooks"][0]["command"],
        "tiered-memory hook session-end"
    );

    // user settings survive a re-install, and no duplicate entries appear
    let mut v = v.clone();
    v["model"] = serde_json::json!("opus");
    std::fs::write(&settings, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    harnesses::install_hooks(claude, &home, &_cwd).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["model"], "opus");
    assert_eq!(v["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    assert_eq!(v["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);

    // removal strips only our entries
    let touched = harnesses::remove_hooks(claude, &home, &_cwd)
        .unwrap()
        .unwrap();
    assert_eq!(touched, settings);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(v["model"], "opus", "user keys untouched");
    assert!(v["hooks"]["SessionStart"].as_array().unwrap().is_empty());
    assert!(v["hooks"]["SessionEnd"].as_array().unwrap().is_empty());
    assert!(harnesses::remove_hooks(claude, &home, &_cwd)
        .unwrap()
        .is_none());
}

#[test]
fn zcode_hooks_enable_the_runner_and_skip_session_end() {
    let (_d, home, _cwd) = temp();
    let known: &'static [harnesses::Harness] = harnesses::KNOWN;
    let agents = known.iter().find(|h| h.id == "agents").unwrap();
    let config = home.join(".zcode/cli/config.json");

    // ZCode speaks through the `agents` entry (Agent Skills spec)
    harnesses::install_hooks(agents, &home, &_cwd).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        v["hooks"]["enabled"], true,
        "config hooks need the runner enabled"
    );
    let start = &v["hooks"]["events"]["SessionStart"];
    assert_eq!(start[0]["matcher"], "startup|resume");
    assert_eq!(
        start[0]["hooks"][0]["command"],
        "tiered-memory hook session-start --format json"
    );
    assert!(
        v["hooks"]["events"].get("SessionEnd").is_none(),
        "ZCode has no SessionEnd event — we must not register one"
    );

    let kind = harnesses::hook_support(agents, &home).unwrap().0;
    assert_eq!(kind, HookKind::Zcode);
}

#[test]
fn unsupported_harnesses_have_no_hooks() {
    let (_d, home, _cwd) = temp();
    let known: &'static [harnesses::Harness] = harnesses::KNOWN;
    let junie = known.iter().find(|h| h.id == "junie").unwrap();
    assert!(harnesses::hook_support(junie, &home).is_none());
    assert!(harnesses::install_hooks(junie, &home, &_cwd)
        .unwrap()
        .is_none());
}
