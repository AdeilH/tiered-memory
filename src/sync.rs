//! The `/tiered-memory` update pipeline: **gather** the current state of all
//! three layers, **extract** new/changed knowledge from a conversation with
//! one LLM call (any OpenAI-compatible endpoint), and **apply** it back into
//! the right layers.
//!
//! Layer routing rules given to the LLM mirror the engine's own semantics:
//! L1 = specific to this project, L2 = belongs to related scopes (sibling
//! components, similar projects), L3 = durable user-level traits. Parameter
//! updates ride as structured `params` with a `key`, so re-assertions upsert.

use crate::engine::{MemoryContext, MemoryEngine};
use crate::error::{MemoryError, Result};
use crate::llm::LlmClient;
use crate::types::{Level, MemoryKind, ParamValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const EXTRACTION_SYSTEM_PROMPT: &str = r#"You maintain a layered long-term memory for a learner, organized like a CPU cache:

- L1 (hot, project-local): preferences and facts specific to THE CURRENT PROJECT only. Example: "In this project the learner wants pure theory, no code examples".
- L2 (warm, related scopes): knowledge that belongs to RELATED scopes — other components of the same product (frontend/backend), similar projects, or the project's L2 GROUP (a named family of projects, listed below as "L2 GROUP" when one is assigned). Use it when the insight matters to sibling work but not to the learner everywhere; L2 entries written for this project become visible to its group-mates and similar projects.
- L3 (cold, global traits): durable user-level traits that hold across ALL projects. Example: "Learner is strong in Python", "consistently prefers slow pace", "likes analogies from games".

You will receive: the CURRENT memory state of all three layers (so you never re-assert what is already known), and a NEW conversation segment.

Extract ONLY genuinely new or CHANGED knowledge from the conversation. Rules:
- Never repeat an existing memory verbatim or trivially rephrased. If the conversation adds nothing, return an empty list.
- Prefer updating parameters: for anything tunable (difficulty, pace, lesson_style, analogy_domain, code_example_density, language, chart_lib, …), emit a params entry with the parameter key. Re-asserting a key updates it; use L1 for project-local values and L3 for values true across projects.
- text: one short third-person sentence, self-contained ("The learner prefers worked examples over lectures").
- topic: for L2 entries, a short kebab-case category used to file the memory into per-topic docs (examples: "writing-style", "flow", "preferences", "tooling", "architecture"). Reuse a category that already exists when it fits. Omit for L1/L3.
- key: required when params is present — the canonical parameter key this entry asserts.
- confidence: 0.5 (hint) to 1.0 (explicit, repeated).
- Route conservatively: when unsure between L1 and L3, prefer L1; L3 only for traits that clearly generalize.

Respond with ONLY a JSON object, no prose:
{"updates": [{"level": "L1", "text": "...", "key": "difficulty", "params": {"difficulty": 0.4}, "confidence": 0.8}, {"level": "L2", "text": "The learner writes concise commit messages", "topic": "writing-style", "confidence": 0.8}]}
"#;

#[derive(Debug, Clone)]
pub struct SyncInput {
    pub user: String,
    pub project_id: String,
    /// The new material to learn from — a transcript, chat log, or summary.
    pub conversation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncEntry {
    pub level: Level,
    pub text: String,
    /// Category slug for L2 entries ("writing-style", …) — files the record
    /// into per-topic docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
    pub confidence: f32,
}

/// The read-only result of gather + extract: what *would* be written.
#[derive(Debug, Clone, Serialize)]
pub struct SyncPlan {
    pub entries: Vec<SyncEntry>,
    pub raw_model_reply: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SyncReport {
    pub stored: Vec<SyncEntry>,
    pub skipped: Vec<String>,
    /// The adjusted parameter set AFTER applying the updates.
    pub params_after: BTreeMap<String, ParamValue>,
    pub raw_model_reply: String,
}

#[derive(Deserialize)]
struct Extraction {
    #[serde(default)]
    updates: Vec<RawUpdate>,
}

#[derive(Deserialize)]
struct RawUpdate {
    level: String,
    text: String,
    #[serde(default)]
    topic: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    params: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default)]
    confidence: Option<f32>,
}

/// Render the gathered 3-layer state as the user half of the LLM prompt.
pub fn render_context(ctx: &MemoryContext) -> String {
    let mut out = String::from("CURRENT MEMORY STATE\n");
    let layer = |name: &str, lines: &[crate::engine::MemoryLine]| -> String {
        if lines.is_empty() {
            return format!("\n{name}: (empty)\n");
        }
        let mut s = format!("\n{name}:\n");
        for l in lines {
            let params = if l.params.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = l
                    .params
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_text()))
                    .collect();
                format!(" [{}]", pairs.join(", "))
            };
            s.push_str(&format!("- {}{}\n", l.text, params));
        }
        s
    };
    out.push_str(&layer("L1 (this project)", &ctx.l1));
    out.push_str(&layer("L2 (related scopes)", &ctx.l2));
    out.push_str(&layer("L3 (global traits)", &ctx.l3));
    if let Some(g) = &ctx.group {
        let members = if ctx.group_members.is_empty() {
            String::from("(none yet)")
        } else {
            ctx.group_members.join(", ")
        };
        out.push_str(&format!("\nL2 GROUP: {g} (member projects: {members})\n"));
    }
    if !ctx.params.is_empty() {
        out.push_str("\nCURRENT ADJUSTED PARAMETERS:\n");
        for p in &ctx.params {
            out.push_str(&format!(
                "- {} = {} [{:?}]\n",
                p.key,
                p.value.as_text(),
                p.source
            ));
        }
    }
    out
}

fn coerce_params(raw: &BTreeMap<String, serde_json::Value>) -> BTreeMap<String, ParamValue> {
    raw.iter()
        .map(|(k, v)| {
            let value = if let Some(b) = v.as_bool() {
                ParamValue::Bool(b)
            } else if let Some(n) = v.as_f64() {
                ParamValue::Number(n)
            } else {
                ParamValue::Text(v.to_string().trim_matches('"').to_string())
            };
            (k.clone(), value)
        })
        .collect()
}

/// Gather all three layers and run the LLM extraction — **read-only**.
/// Call [`apply`] to write the plan, or [`sync`] for both in one call.
pub fn plan(engine: &MemoryEngine, llm: &LlmClient, input: &SyncInput) -> Result<SyncPlan> {
    if input.conversation.trim().is_empty() {
        return Err(MemoryError::invalid(
            "nothing to sync — pass the conversation/transcript (--file, --stdin or --text)",
        ));
    }

    // 1) gather all three layers
    let ctx = engine.memory_context(&input.user, Some(&input.project_id), 40)?;

    // 2) one LLM extraction call
    let user_prompt = format!(
        "{}\n---\nCURRENT PROJECT: {}\n\nNEW CONVERSATION SEGMENT:\n{}",
        render_context(&ctx),
        input.project_id,
        input.conversation.trim()
    );
    let reply = llm.chat(EXTRACTION_SYSTEM_PROMPT, &user_prompt)?;
    let parsed: Extraction = crate::llm::extract_json(&reply)
        .and_then(|v| serde_json::from_value(v).ok())
        .ok_or_else(|| {
            MemoryError::Embedder(format!(
                "could not parse the LLM's JSON reply: {}",
                reply.chars().take(200).collect::<String>()
            ))
        })?;

    let mut plan = SyncPlan {
        raw_model_reply: reply,
        entries: Vec::new(),
    };
    for u in parsed.updates {
        if u.text.trim().is_empty() {
            continue;
        }
        let level = match u.level.to_ascii_uppercase().as_str() {
            "L1" | "1" => Level::L1,
            "L2" | "2" => Level::L2,
            "L3" | "3" => Level::L3,
            _ => continue, // unknown level — dropped at plan time
        };
        let params = u.params.as_ref().map(coerce_params);
        let key = u.key.clone().or_else(|| {
            params.as_ref().and_then(|p| {
                if p.len() == 1 {
                    p.keys().next().cloned()
                } else {
                    None
                }
            })
        });
        plan.entries.push(SyncEntry {
            level,
            text: u.text.trim().to_string(),
            topic: u.topic.as_deref().and_then(crate::store::normalize_topic),
            key,
            params: params.unwrap_or_default(),
            confidence: u.confidence.unwrap_or(0.75).clamp(0.3, 1.0),
        });
    }
    Ok(plan)
}

/// Write a plan through the engine's own write path (key upserts, content
/// dedupe, capacity, persistence all apply). Skipped entries carry reasons.
pub fn apply(engine: &MemoryEngine, input: &SyncInput, plan: &SyncPlan) -> Result<SyncReport> {
    let mut report = SyncReport {
        raw_model_reply: plan.raw_model_reply.clone(),
        ..Default::default()
    };
    for entry in &plan.entries {
        let has_params = !entry.params.is_empty();
        let outcome = engine.remember(crate::engine::RememberInput {
            user: input.user.clone(),
            text: entry.text.clone(),
            project_id: if entry.level == Level::L3 {
                None
            } else {
                Some(input.project_id.clone())
            },
            kind: Some(if has_params {
                MemoryKind::Feedback
            } else {
                MemoryKind::Note
            }),
            params: if has_params {
                Some(entry.params.clone())
            } else {
                None
            },
            key_hint: entry.key.clone(),
            confidence: Some(entry.confidence),
            pinned: None,
            ttl_days: None,
            level: Some(entry.level),
            group: None,
            topic: entry.topic.clone(),
        });
        match outcome {
            Ok(_) => report.stored.push(entry.clone()),
            Err(e) => report.skipped.push(format!("{} ({e})", entry.text)),
        }
    }
    for s in engine.adjusted_parameters(&input.user, Some(&input.project_id))? {
        report.params_after.insert(s.key, s.value);
    }
    Ok(report)
}

/// Convenience: gather → extract → apply in one call against one engine.
pub fn sync(engine: &MemoryEngine, llm: &LlmClient, input: &SyncInput) -> Result<SyncReport> {
    apply(engine, input, &plan(engine, llm, input)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_context_lists_all_layers() {
        let ctx = MemoryContext {
            l1: vec![crate::engine::MemoryLine {
                id: "m1".into(),
                text: "wants pure theory".into(),
                params: BTreeMap::new(),
                key_hint: None,
                confidence: 0.8,
                pinned: false,
            }],
            l2: vec![],
            l3: vec![crate::engine::MemoryLine {
                id: "m2".into(),
                text: "strong in Python".into(),
                params: BTreeMap::new(),
                key_hint: None,
                confidence: 0.9,
                pinned: false,
            }],
            params: vec![],
            group: Some("rust-clis".into()),
            group_members: vec!["arg-parser".into()],
        };
        let s = render_context(&ctx);
        assert!(s.contains("L1 (this project)"));
        assert!(s.contains("wants pure theory"));
        assert!(s.contains("L2 (related scopes): (empty)"));
        assert!(s.contains("strong in Python"));
        assert!(s.contains("L2 GROUP: rust-clis"));
        assert!(s.contains("arg-parser"));
    }
}
