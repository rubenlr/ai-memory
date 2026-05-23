//! `ai-memory install-hooks` — print the suggested lifecycle-hook
//! configuration for the chosen agent CLI.
//!
//! In M3 this is *non-destructive*: we render the JSON snippet the user
//! should merge into their agent CLI's settings file, plus the absolute
//! paths to the vendored shell scripts. We intentionally do not mutate
//! `~/.claude/settings.json` automatically — agent CLI hook formats are
//! still in flux and bad merges are very user-visible.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentChoice, InstallHooksArgs};
use crate::commands::apply_shared::{ApplyOutcome, apply_atomic, mutate_json};
use crate::commands::render_shared::{build_claude_code_payload, build_codex_payload};
use crate::config::Config;

/// Run the `install-hooks` subcommand.
///
/// # Errors
/// Returns an error if the hook script directory cannot be located.
pub fn run(_config: &Config, args: InstallHooksArgs) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(args.hooks_dir.as_deref(), args.agent)?;
    let auth = args.auth_token.as_deref();
    if args.apply {
        return match args.agent {
            AgentChoice::ClaudeCode => {
                apply_to_claude_code_settings(&hooks_dir, &args.server_url, auth, &args)
            }
            AgentChoice::Codex => {
                apply_to_codex_settings(&hooks_dir, &args.server_url, auth, &args)
            }
            AgentChoice::OpenCode => bail!(
                "--apply does not yet support --agent open-code (no stable upstream \
                 hook schema documented). Run without --apply to print the snippet."
            ),
        };
    }
    match args.agent {
        AgentChoice::ClaudeCode => render_claude_code(&hooks_dir, &args.server_url, auth),
        AgentChoice::Codex => render_agent("codex", &hooks_dir, &args.server_url, auth),
        AgentChoice::OpenCode => render_agent("opencode", &hooks_dir, &args.server_url, auth),
    }
}

/// Mutate `~/.claude/settings.json` in place: replace the seven hook
/// entries ai-memory cares about; preserve every other hook the user
/// has wired up to other tools.
fn apply_to_claude_code_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => dirs::home_dir()
            .context("could not locate $HOME for ~/.claude/settings.json")?
            .join(".claude")
            .join("settings.json"),
    };
    let payload = build_claude_code_payload(hooks_dir, server_url, auth_token);
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: build_claude_code_payload didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            // Get-or-create the top-level `hooks` table, then OVERLAY
            // our seven event keys onto the user's table. Anything
            // they had under a non-overlapping event name (e.g. a
            // hand-written "Notification" hook) survives.
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in settings.json but not an object")?;
            for (event, value) in &our_hooks {
                hooks.insert(event.clone(), value.clone());
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    Ok(())
}

/// Mutate `~/.codex/hooks.json` (creating it if absent) so Codex's
/// lifecycle hook runner fires the ai-memory scripts on every
/// session/prompt/tool event.
///
/// Codex's hook config is structurally identical to Claude Code's
/// (verified against `openai/codex/codex-rs/config/src/hooks_tests.rs`):
///
///   { "hooks": {
///       "SessionStart": [
///         { "matcher": "",
///           "hooks": [ {"type":"command", "command":"..."} ]
///         }
///       ], ...
///   } }
///
/// Codex looks for hooks in `~/.codex/hooks.json` by default (or
/// wherever `hooks = "./relative-path.json"` in config.toml points).
/// We write the standalone file and don't touch config.toml — Codex
/// picks it up automatically.
///
/// Trust note: Codex refuses to RUN new hooks until the user accepts
/// them in the TUI ("Trust all and continue") or sets
/// `--dangerously-bypass-hook-trust`. We print a reminder.
fn apply_to_codex_settings(
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
    args: &InstallHooksArgs,
) -> Result<()> {
    let path = match &args.config_file {
        Some(p) => p.clone(),
        None => dirs::home_dir()
            .context("could not locate $HOME for ~/.codex/hooks.json")?
            .join(".codex")
            .join("hooks.json"),
    };
    // Build the Codex-flavoured payload. The JSON shape is identical
    // to Claude Code's matcher + nested hooks form — only the event
    // list differs (no `SessionEnd`, which Codex doesn't recognise).
    let payload = build_codex_payload(hooks_dir, server_url, auth_token);
    let our_hooks = payload
        .get("hooks")
        .and_then(|v| v.as_object())
        .context("internal: payload builder didn't return a hooks object")?
        .clone();
    let outcome = apply_atomic(&path, |existing| {
        mutate_json(existing, |root| {
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
                .as_object_mut()
                .context("`hooks` is present in hooks.json but not an object")?;
            // Remove any stale `SessionEnd` entry left behind by an
            // earlier version of install-hooks that mistakenly wrote
            // the Claude-Code-only event into Codex's file. Codex
            // ignores unknown events but the file looks cleaner
            // without dead keys.
            hooks.remove("SessionEnd");
            for (event, value) in &our_hooks {
                hooks.insert(event.clone(), value.clone());
            }
            Ok(())
        })
    })?;
    println!(
        "✓ {} {} ({})",
        outcome.verb(),
        path.display(),
        match outcome {
            ApplyOutcome::Created => "new file",
            ApplyOutcome::Updated => "backup written next to it",
            ApplyOutcome::NoOp => "already up to date",
        }
    );
    // First-time trust reminder. Codex's TUI flags new/changed
    // hooks on startup; users must explicitly trust them before
    // they fire.
    if !matches!(outcome, ApplyOutcome::NoOp) {
        println!();
        println!("Codex requires explicit trust for new hooks. Next time you start `codex`:");
        println!("  → the TUI will surface 'Hooks need review' for each new event");
        println!("  → choose 'Trust all and continue' (or trust individually)");
        println!("To bypass the prompt for automated installs, start with");
        println!("`codex --dangerously-bypass-hook-trust` (review hook scripts first).");
    }
    Ok(())
}

fn render_agent(
    label: &str,
    hooks_dir: &Path,
    server_url: &str,
    auth_token: Option<&str>,
) -> Result<()> {
    println!("# {label} hook scripts (manual install — wire each to the matching event)");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: set AI_MEMORY_AUTH_TOKEN in each hook's environment to the");
        println!("#       value passed via --auth-token (omitted from this printout).");
    } else {
        println!("# Auth: server requires no bearer token. To require one, generate a");
        println!("#       token with `ai-memory generate-auth-token` and pass it via");
        println!("#       --auth-token here AND set AI_MEMORY_AUTH_TOKEN on the server.");
    }
    println!();
    for entry in std::fs::read_dir(hooks_dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file() && p.extension().is_some_and(|e| e == "sh") {
            println!("- {}", p.display());
        }
    }
    println!();
    println!("Set AI_MEMORY_HOOK_URL in each hook's environment to override the default.");
    Ok(())
}

fn resolve_hooks_dir(explicit: Option<&Path>, agent: AgentChoice) -> Result<PathBuf> {
    let sub = match agent {
        AgentChoice::ClaudeCode => "claude-code",
        AgentChoice::Codex => "codex",
        AgentChoice::OpenCode => "opencode",
    };
    if let Some(p) = explicit {
        let path = p.join(sub);
        if path.is_dir() {
            return Ok(path);
        }
        anyhow::bail!("hooks directory {} does not exist", path.display());
    }

    // Probe candidates in order. The first dir that exists wins.
    let candidates: [PathBuf; 3] = [
        // Cargo-run from the repo.
        repo_root_guess()
            .map(|r| r.join("hooks").join(sub))
            .unwrap_or_default(),
        // Docker image lays them out under /usr/local/share/ai-memory/.
        PathBuf::from(format!("/usr/local/share/ai-memory/hooks/{sub}")),
        // Local install honourable mention.
        dirs::data_local_dir()
            .map(|d| d.join("ai-memory/hooks").join(sub))
            .unwrap_or_default(),
    ];
    for path in &candidates {
        if !path.as_os_str().is_empty() && path.is_dir() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!("could not locate hooks directory. Tried: {:?}", candidates,);
}

fn repo_root_guess() -> Option<PathBuf> {
    // When the binary lives under target/{debug,release}/<name>, the
    // workspace root is two parents up.
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent()?.parent()?.parent().map(Path::to_path_buf))
}

// CLAUDE_CODE_EVENTS + build_claude_code_payload now live in
// `super::render_shared`, shared with `setup-agent`.

fn render_claude_code(hooks_dir: &Path, server_url: &str, auth_token: Option<&str>) -> Result<()> {
    // Soft check: warn (don't bail) if a script is missing. The user
    // may be running this command inside docker against a host path
    // that exists only on the host's filesystem — bailing would
    // sabotage the docker-only flow `setup-agent` enables.
    for (_, script) in super::render_shared::CLAUDE_CODE_EVENTS {
        let abs = hooks_dir.join(script);
        if !abs.exists() {
            eprintln!(
                "# warning: {} not present on this filesystem. \
                 If this command is running inside docker against a \
                 host path, you can ignore this; otherwise extract \
                 the scripts first with `ai-memory setup-agent`.",
                abs.display()
            );
        }
    }
    let payload = build_claude_code_payload(hooks_dir, server_url, auth_token);
    let serialized =
        serde_json::to_string_pretty(&payload).context("serializing claude code hook config")?;
    println!("# Claude Code hook config — merge into ~/.claude/settings.json");
    println!("# Hook scripts: {}", hooks_dir.display());
    println!("# AI-memory server URL: {server_url}");
    if auth_token.is_some() {
        println!("# Auth: AI_MEMORY_AUTH_TOKEN embedded in each hook's env block below.");
        println!("#       Treat ~/.claude/settings.json as sensitive (chmod 600).");
    }
    println!();
    println!("{serialized}");
    Ok(())
}
