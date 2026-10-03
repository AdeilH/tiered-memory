//! LLM provider configuration (`credentials`), the HTTP service bearer token
//! (`auth`), and the provider's model catalog (`models`).

use std::io::Write;

use crate::{arg_value, cmd_args, data_root};
use tiered_memory::LlmConfig;

// -- credentials -------------------------------------------------------------

pub(crate) fn credentials() -> Result<(), String> {
    let args = cmd_args();
    match args.first().map(String::as_str) {
        // bare `credentials` = the common path: interactive setup
        None | Some("set") => credentials_set(&args),
        Some("show") => credentials_show(),
        Some("clear") => credentials_clear(),
        Some(other) => Err(format!(
            "unknown credentials subcommand `{other}` (setup | show | clear)"
        )),
    }
}

pub(crate) fn credentials_set(args: &[String]) -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    let has_flags = arg_value(args, "--base-url").is_some()
        || arg_value(args, "--api-key").is_some()
        || arg_value(args, "--model").is_some()
        || arg_value(args, "--temperature").is_some();

    let mut config = LlmConfig::load_from(&path)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    if has_flags {
        // scriptable path — exactly what the flags say, nothing else
        if let Some(v) = arg_value(args, "--base-url") {
            config.base_url = v;
        }
        if let Some(v) = arg_value(args, "--api-key") {
            config.api_key = Some(v);
        }
        if let Some(v) = arg_value(args, "--model") {
            config.model = v;
        }
        if let Some(v) = arg_value(args, "--temperature") {
            config.temperature = v.parse::<f32>().ok();
        }
    } else if crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        // interactive wizard: provider → key → searchable model list
        if !tiered_memory::tui::run_credentials_wizard(&mut config).map_err(|e| e.to_string())? {
            println!("cancelled — credentials unchanged");
            return Ok(());
        }
    } else {
        return Err("no TTY — pass --base-url/--api-key/--model or run from a terminal".into());
    }

    config.save_to(&path).map_err(|e| e.to_string())?;
    println!("credentials written to {}", path.display());
    println!("  base_url: {}", config.base_url);
    println!("  model:    {}", config.model);
    println!(
        "  api_key:  {}",
        tiered_memory::llm::mask_key(config.api_key.as_deref().unwrap_or("(none)"))
    );
    Ok(())
}

fn credentials_show() -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    let config = LlmConfig::resolve(None, &data_root()).map_err(|e| e.to_string())?;
    match config {
        Some(c) => {
            let source = if path.is_file() {
                path.display().to_string()
            } else {
                "environment (TM_LLM_*)".to_string()
            };
            println!("LLM credentials (from {source}):");
            println!("  base_url: {}", c.base_url);
            println!("  model:    {}", c.model);
            println!(
                "  api_key:  {}",
                tiered_memory::llm::mask_key(c.api_key.as_deref().unwrap_or("(none)"))
            );
        }
        None => {
            println!("no LLM credentials configured.");
            println!("run the interactive setup with:");
            println!("  tiered-memory credentials");
            println!("or non-interactively:");
            println!(
                "  tiered-memory credentials set --base-url https://api.openai.com/v1 --api-key sk-... --model gpt-4o-mini"
            );
            println!("(env vars TM_LLM_BASE_URL / TM_LLM_API_KEY / TM_LLM_MODEL also work)");
        }
    }
    Ok(())
}

fn credentials_clear() -> Result<(), String> {
    let path = data_root().join(tiered_memory::CREDENTIALS_FILE);
    match std::fs::remove_file(&path) {
        Ok(_) => println!("credentials removed ({})", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no credentials file at {}", path.display())
        }
        Err(e) => return Err(format!("remove {}: {e}", path.display())),
    }
    Ok(())
}

// -- auth --------------------------------------------------------------------

/// `tiered-memory auth on|off|show` — bearer-token auth for the HTTP service.
/// `on` generates a high-entropy token, stores it at `{data}/token` (0600)
/// and prints it once; `serve` enforces it on next start.
pub(crate) fn auth() -> Result<(), String> {
    let args = cmd_args();
    let sub = args.first().map(String::as_str).unwrap_or("show");
    let path = data_root().join("token");

    match sub {
        "on" => auth_on(&path)?,
        "off" => match std::fs::remove_file(&path) {
            Ok(_) => println!("auth OFF — token removed ({})", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("auth already OFF — no token file at {}", path.display())
            }
            Err(e) => return Err(format!("remove {}: {e}", path.display())),
        },
        _ => match std::fs::read_to_string(&path) {
            Ok(t) => {
                let t = t.trim();
                println!(
                    "auth ON — token {} in {}",
                    tiered_memory::llm::mask_key(t),
                    path.display()
                );
                println!(
                    "clients send: Authorization: Bearer <token>; restart serve after changes"
                );
            }
            Err(_) => {
                println!("auth OFF — no token file at {}", path.display());
                println!("turn it on with: tiered-memory auth on");
            }
        },
    }
    Ok(())
}

fn auth_on(path: &std::path::Path) -> Result<(), String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| format!("entropy source: {e}"))?;
    let token: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        f.write_all(token.as_bytes())
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, &token).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!(
        "auth ON — token written to {} (perms 0600, shown once):",
        path.display()
    );
    println!("  {token}");
    println!("clients send: Authorization: Bearer <token>");
    println!("restart `tiered-memory serve` to enforce it");
    Ok(())
}

// -- models ------------------------------------------------------------------

/// `tiered-memory models` — list the configured provider's model catalog
/// (the same data the wizard's searchable picker shows).
pub(crate) fn models() -> Result<(), String> {
    let config = LlmConfig::resolve(None, &data_root())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no LLM credentials — run `tiered-memory credentials` first".to_string())?;
    let list = tiered_memory::llm::fetch_models(&config.base_url, config.api_key.as_deref())
        .map_err(|e| e.to_string())?;
    println!("models at {} ({}):", config.base_url, list.len());
    for m in &list {
        let cur = if *m == config.model {
            "  ← current"
        } else {
            ""
        };
        println!("  {m}{cur}");
    }
    Ok(())
}
