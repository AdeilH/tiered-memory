//! Interactive terminal UI for the credentials wizard — a lightweight
//! crossterm form (no full-screen framework): a searchable provider picker, a
//! masked API-key input, and a **searchable model selector fed by the
//! provider's `/models` endpoint**.
//!
//! All interactive functions return `Ok(None)` when the user cancels (Esc /
//! Ctrl-C), and the caller decides what "keep current" means.

use crate::error::{MemoryError, Result};
use crossterm::cursor::MoveTo;
use crossterm::event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
};
use std::io::{stdout, Write};
use std::time::Duration;

/// Known OpenAI-compatible providers offered in the wizard.
pub struct ProviderPreset {
    pub name: &'static str,
    pub base_url: &'static str,
    /// Local servers accept any key; the wizard softens the prompt for them.
    pub local: bool,
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        local: false,
    },
    ProviderPreset {
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        local: false,
    },
    ProviderPreset {
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        local: false,
    },
    ProviderPreset {
        name: "Ollama (local)",
        base_url: "http://localhost:11434/v1",
        local: true,
    },
    ProviderPreset {
        name: "LM Studio (local)",
        base_url: "http://localhost:1234/v1",
        local: true,
    },
    ProviderPreset {
        name: "vLLM (local)",
        base_url: "http://localhost:8000/v1",
        local: true,
    },
];

// -- terminal plumbing -------------------------------------------------------

/// Raw-mode + alternate-screen guard, shared with `harnesses.rs`'s picker:
/// enables raw mode on [`enter`](RawGuard::enter), restores the terminal on
/// drop (including through the picker's `?` error paths).
pub(crate) struct RawGuard;

impl RawGuard {
    pub(crate) fn enter() -> Result<Self> {
        enable_raw_mode().map_err(|e| MemoryError::invalid(format!("terminal: {e}")))?;
        execute!(stdout(), EnterAlternateScreen)
            .map_err(|e| MemoryError::invalid(format!("terminal: {e}")))?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = stdout().flush();
    }
}

/// Terminal width in columns, capped at 120 (the widest layout the wizards
/// draw). Shared with `harnesses.rs`'s picker so rows never exceed the screen.
pub(crate) fn width() -> u16 {
    crossterm::terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80)
        .min(120)
}

/// Terminal height in rows.
pub(crate) fn height() -> u16 {
    crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24)
}

pub(crate) fn trunc(s: &str, max: u16) -> String {
    let max = max as usize;
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep = max.saturating_sub(1);
        s.chars().take(keep).collect::<String>() + "…"
    }
}

/// Draw one line with optional color and emphasis, clearing the rest of it.
pub(crate) fn draw(y: u16, text: &str, fg: Option<Color>, emphasis: Option<Attribute>) {
    let _ = execute!(
        stdout(),
        MoveTo(0, y),
        SetForegroundColor(fg.unwrap_or(Color::Reset)),
        SetAttribute(emphasis.unwrap_or(Attribute::NormalIntensity)),
        Print(trunc(text, width())),
        ResetColor,
        SetAttribute(Attribute::NormalIntensity),
        Clear(ClearType::UntilNewLine)
    );
}

pub(crate) fn clear_screen() {
    let _ = execute!(stdout(), Clear(ClearType::All), MoveTo(0, 0));
}

pub(crate) fn header(title: &str) {
    draw(
        0,
        &format!(" tiered-memory · {title} "),
        None,
        Some(Attribute::Reverse),
    );
}

fn footer(y: u16, hint: &str) {
    let row = y.saturating_sub(1);
    draw(row, &format!(" {hint}"), Some(Color::DarkGrey), None);
}

const FILTER_PREFIX: &str = "search: ";

/// Searchable single-select list. `entries` are (label, hint) pairs; typing
/// filters (case-insensitive substring), ↑/↓ move, Enter picks, Esc cancels.
pub fn select_from_list(
    title: &str,
    entries: &[(String, String)],
    preselect: Option<usize>,
) -> Result<Option<usize>> {
    let _guard = RawGuard::enter()?;
    let mut filter = String::new();
    let mut selection = preselect.unwrap_or(0);
    let visible_rows = 14usize;
    // draw only when something changed (poll timeouts must not flicker)
    let mut redraw = true;
    let mut filtered: Vec<usize> = Vec::new();

    loop {
        // filtering is cheap; run it whenever state may have changed so the
        // event handler below can always rely on `filtered`
        if redraw {
            filtered = entries
                .iter()
                .enumerate()
                .filter(|(_, (label, hint))| {
                    let hay = format!("{label} {hint}").to_lowercase();
                    hay.contains(&filter.to_lowercase())
                })
                .map(|(i, _)| i)
                .collect();

            // keep the selection on a visible/valid filtered item
            if !filtered.is_empty() && !filtered.contains(&selection) {
                let pos = filtered.iter().position(|&i| i >= selection);
                selection = match pos {
                    Some(p) => filtered[p],
                    None => *filtered.last().unwrap(),
                };
            }
        }

        if redraw {
            redraw = false;
            clear_screen();
            header(title);
            draw(
                2,
                &format!("{FILTER_PREFIX}{filter}"),
                Some(Color::Cyan),
                None,
            );

            if filtered.is_empty() {
                draw(4, "  (no matches)", Some(Color::DarkRed), None);
            } else {
                let pos = filtered.iter().position(|&i| i == selection).unwrap_or(0);
                let start = pos.saturating_sub(visible_rows / 2);
                for (row, &idx) in filtered.iter().skip(start).take(visible_rows).enumerate() {
                    let (label, hint) = &entries[idx];
                    let line = if hint.is_empty() {
                        format!("  {label}")
                    } else {
                        format!("  {label:<28} {hint}")
                    };
                    if idx == selection {
                        draw(
                            4 + row as u16,
                            &format!("▸{line}"),
                            Some(Color::Cyan),
                            Some(Attribute::Bold),
                        );
                    } else {
                        draw(4 + row as u16, &line, None, None);
                    }
                }
                if filtered.len() > visible_rows {
                    draw(
                        4 + visible_rows as u16,
                        &format!("  … {} more", filtered.len() - visible_rows),
                        Some(Color::DarkGrey),
                        None,
                    );
                }
            }
            footer(22, "↑/↓ move · type to search · Enter select · Esc cancel");
            stdout().flush().ok();
        }

        if !event_available()? {
            continue;
        }
        redraw = true;
        match read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c'))
                {
                    return Ok(None);
                }
                match k.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter => {
                        if filtered.is_empty() {
                            continue;
                        }
                        return Ok(Some(selection));
                    }
                    KeyCode::Up => {
                        if let Some(p) = filtered.iter().position(|&i| i == selection) {
                            if p > 0 {
                                selection = filtered[p - 1];
                            }
                        }
                    }
                    KeyCode::Down => {
                        if let Some(p) = filtered.iter().position(|&i| i == selection) {
                            if p + 1 < filtered.len() {
                                selection = filtered[p + 1];
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                    }
                    KeyCode::Char(c) => filter.push(c),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Single-line editor. `mask` renders `*` per character (API keys).
/// Enter returns the current text; Esc/Ctrl-C cancels.
pub fn input_line(
    title: &str,
    prompt: &str,
    initial: &str,
    mask: bool,
    hint: Option<&str>,
) -> Result<Option<String>> {
    let _guard = RawGuard::enter()?;
    let mut text: Vec<char> = initial.chars().collect();
    let mut cursor = text.len();
    let mut redraw = true;

    loop {
        if redraw {
            redraw = false;
            clear_screen();
            header(title);
            draw(2, prompt, None, Some(Attribute::Bold));
            let shown: String = if mask {
                "*".repeat(text.len())
            } else {
                text.iter().collect()
            };
            draw(4, &format!(" > {shown}"), Some(Color::Cyan), None);
            if let Some(h) = hint {
                draw(6, h, Some(Color::DarkGrey), None);
            }
            footer(14, "Enter confirm · Esc cancel");
        }
        // terminal cursor inside the field (moves on every keystroke)
        let _ = execute!(stdout(), MoveTo(3 + cursor as u16, 4));
        stdout().flush().ok();

        if !event_available()? {
            continue;
        }
        match read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(k.code, KeyCode::Char('c'))
                {
                    return Ok(None);
                }
                let mut changed = false;
                match k.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter => return Ok(Some(text.iter().collect())),
                    KeyCode::Left => {
                        cursor = cursor.saturating_sub(1);
                        changed = true;
                    }
                    KeyCode::Right => {
                        cursor = (cursor + 1).min(text.len());
                        changed = true;
                    }
                    KeyCode::Home => {
                        cursor = 0;
                        changed = true;
                    }
                    KeyCode::End => {
                        cursor = text.len();
                        changed = true;
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            cursor -= 1;
                            text.remove(cursor);
                            changed = true;
                        }
                    }
                    KeyCode::Delete => {
                        if cursor < text.len() {
                            text.remove(cursor);
                            changed = true;
                        }
                    }
                    KeyCode::Char(c) => {
                        text.insert(cursor, c);
                        cursor += 1;
                        changed = true;
                    }
                    _ => {}
                }
                redraw = changed;
            }
            _ => {}
        }
    }
}

/// Wait for a key/resize event without busy-looping.
pub(crate) fn event_available() -> Result<bool> {
    poll(Duration::from_millis(250)).map_err(|e| MemoryError::invalid(format!("terminal: {e}")))
}

// -- credentials wizard ------------------------------------------------------

/// Full interactive flow: provider → API key → model (fetched from the
/// provider and searched). Mutates `cfg` in place only after all steps
/// complete; returns false when the user cancelled at any point.
pub fn run_credentials_wizard(cfg: &mut crate::llm::LlmConfig) -> Result<bool> {
    let mut proposed = cfg.clone();

    // 1) provider / base URL
    let mut entries: Vec<(String, String)> = Vec::new();
    if !proposed.base_url.is_empty() {
        entries.push(("Keep current".to_string(), proposed.base_url.clone()));
    }
    for p in PROVIDER_PRESETS {
        entries.push((p.name.to_string(), p.base_url.to_string()));
    }
    entries.push(("Custom base URL…".to_string(), String::new()));

    let preselect = if proposed.base_url.is_empty() {
        None
    } else {
        Some(0)
    };
    let Some(pick) = select_from_list("LLM credentials · provider", &entries, preselect)? else {
        return Ok(false);
    };
    let pick_index = if proposed.base_url.is_empty() {
        pick
    } else if pick == 0 {
        usize::MAX // "keep current"
    } else {
        pick - 1
    };

    match pick_index {
        usize::MAX => {} // keep current base_url
        i if i < PROVIDER_PRESETS.len() => {
            proposed.base_url = PROVIDER_PRESETS[i].base_url.to_string();
        }
        _ => {
            let Some(custom) = input_line(
                "LLM credentials · base URL",
                "OpenAI-compatible base URL (ends in /v1):",
                &proposed.base_url,
                false,
                Some("examples: https://api.openai.com/v1 · http://localhost:11434/v1 (Ollama)"),
            )?
            else {
                return Ok(false);
            };
            proposed.base_url = custom.trim().trim_end_matches('/').to_string();
        }
    }

    // 2) API key (masked)
    let is_local = PROVIDER_PRESETS
        .iter()
        .any(|p| p.base_url == proposed.base_url && p.local);
    let hint = if is_local {
        Some("local server — Enter to leave the key empty")
    } else if cfg.api_key.is_some() {
        Some("Enter without typing keeps the current key")
    } else {
        Some("input is hidden; leave empty only for local servers")
    };
    let Some(key) = input_line("LLM credentials · API key", "API key:", "", true, hint)? else {
        return Ok(false);
    };
    if !key.trim().is_empty() {
        proposed.api_key = Some(key.trim().to_string());
    } else if !is_local && cfg.api_key.is_none() && proposed.base_url.starts_with("https") {
        proposed.api_key = None; // explicit empty for a remote provider
    }

    // 3) model — fetch the provider's catalog and let the user search it
    let manual_entry = |cfg_model: &str, warning: Option<&str>| -> Result<Option<String>> {
        if let Some(w) = warning {
            let _guard = RawGuard::enter()?;
            clear_screen();
            header("LLM credentials · model");
            draw(2, w, Some(Color::DarkRed), None);
            std::thread::sleep(Duration::from_millis(1200));
        }
        input_line(
            "LLM credentials · model",
            "Model id:",
            cfg_model,
            false,
            Some("e.g. gpt-4o-mini · llama3.2 · qwen2.5-coder-7b-instruct"),
        )
    };

    let model: String =
        match crate::llm::fetch_models(&proposed.base_url, proposed.api_key.as_deref()) {
            Ok(models) => {
                let mut entries: Vec<(String, String)> =
                    vec![("✎ Type a model id manually…".to_string(), String::new())];
                entries.extend(models.iter().map(|m| (m.clone(), String::new())));
                let preselect = models
                    .iter()
                    .position(|m| *m == proposed.model)
                    .map(|p| p + 1);
                let Some(pick) = select_from_list("LLM credentials · model", &entries, preselect)?
                else {
                    return Ok(false);
                };
                if pick == 0 {
                    match manual_entry(&proposed.model, None)? {
                        Some(m) if !m.trim().is_empty() => m.trim().to_string(),
                        _ => return Ok(false),
                    }
                } else {
                    models[pick - 1].clone()
                }
            }
            Err(e) => {
                match manual_entry(
                    &proposed.model,
                    Some(&format!("could not fetch models: {e}")),
                )? {
                    Some(m) if !m.trim().is_empty() => m.trim().to_string(),
                    _ => return Ok(false),
                }
            }
        };
    proposed.model = model;

    // 4) confirm
    let confirm = format!(
        "base: {}\nmodel: {}\nkey:  {}\n\nSave these credentials?",
        proposed.base_url,
        proposed.model,
        match proposed.api_key.as_deref() {
            Some(k) if !k.is_empty() => crate::llm::mask_key(k),
            _ => "(none)".to_string(),
        }
    );
    {
        let _guard = RawGuard::enter()?;
        clear_screen();
        header("LLM credentials · confirm");
        for (i, line) in confirm.lines().enumerate() {
            draw(2 + i as u16, line, None, None);
        }
        draw(
            2 + confirm.lines().count() as u16 + 1,
            "Enter save · Esc cancel",
            Some(Color::Cyan),
            None,
        );
        loop {
            if !event_available()? {
                continue;
            }
            if let Event::Key(KeyEvent {
                code,
                kind: KeyEventKind::Press,
                modifiers,
                ..
            }) = read()?
            {
                if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                    return Ok(false);
                }
                match code {
                    KeyCode::Enter => {
                        *cfg = proposed;
                        return Ok(true);
                    }
                    KeyCode::Esc => return Ok(false),
                    _ => {}
                }
            }
        }
    }
}
