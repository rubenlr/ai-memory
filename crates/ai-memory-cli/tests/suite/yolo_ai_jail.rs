//! Integration coverage for the `--yolo` ai-jail re-exec contract.
//!
//! Unit tests in `ai-memory-workstream::jail` prove the pure argv assembly and
//! per-OS detection. These tests assert the *cross-tool* contract: the argv
//! `build_ai_jail_invocation` produces is one the real `ai-jail` binary accepts
//! and forwards unchanged. The real-`ai-jail` test skips cleanly when ai-jail
//! (or, on Linux, `bwrap`) is not installed, mirroring the opt-in discipline of
//! `tests/e2e/handoff_smoke.sh`, so CI without a sandbox stays green.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use ai_memory_workstream::build_ai_jail_invocation;

fn forwarded() -> Vec<OsString> {
    ["run", "claude", "--yolo"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// ai-jail's parser (README "Positional command behavior is sacred") treats the
/// first positional token as the wrapped program and captures everything after
/// it verbatim. So every sandbox flag we emit must precede the wrapped exe, and
/// the token right after the leading flags must be the exe itself — never a
/// stray value. This is the exact shape a value-taking `--agent-state` would
/// have broken (it would have made ai-jail run the value as the command).
#[test]
fn invocation_puts_all_sandbox_flags_before_the_wrapped_exe() {
    let exe = Path::new("/usr/local/bin/ai-memory");
    let argv = build_ai_jail_invocation(
        exe,
        &forwarded(),
        &["AI_MEMORY_SERVER_URL", "ANTHROPIC_API_KEY"],
        true,
    );
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // The wrapped exe is the first positional; everything before it is a known
    // sandbox flag (or the value of `--env`), and everything from it on is the
    // wrapped command.
    let exe_pos = strs
        .iter()
        .position(|s| s == "/usr/local/bin/ai-memory")
        .expect("wrapped exe present in argv");
    assert_eq!(
        &strs[exe_pos..],
        &["/usr/local/bin/ai-memory", "run", "claude", "--yolo"],
        "the wrapped command must be forwarded verbatim, right after the exe"
    );

    // `--agent-state` is a bare toggle: it must be followed by another flag or
    // the exe, never by a value ai-jail would misread as the command.
    let agent_state = strs
        .iter()
        .position(|s| s == "--agent-state")
        .expect("agent-state flag");
    let after = &strs[agent_state + 1];
    assert!(
        after.starts_with("--") || after == "/usr/local/bin/ai-memory",
        "--agent-state must be a bare toggle, but is followed by {after:?}"
    );
}

/// `--env` is emitted only for names the caller marked present, one flag per
/// name, and never with an inline value (ai-jail forwards the host value for a
/// bare `--env NAME`).
#[test]
fn invocation_emits_one_bare_env_flag_per_present_name() {
    let exe = Path::new("/bin/ai-memory");
    let argv = build_ai_jail_invocation(exe, &forwarded(), &["CLAUDE_CONFIG_DIR"], false);
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let env_flags = strs.iter().filter(|s| *s == "--env").count();
    assert_eq!(env_flags, 1);
    let idx = strs.iter().position(|s| s == "--env").unwrap();
    assert_eq!(strs[idx + 1], "CLAUDE_CONFIG_DIR");
    assert!(
        !strs[idx + 1].contains('='),
        "forward the name, not name=value"
    );
    // agent_state=false → no toggle present.
    assert!(!strs.iter().any(|s| s == "--agent-state"));
}

/// Locate `ai-jail` the same way the feature does; `None` ⇒ skip.
fn ai_jail_path() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("ai-jail");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let candidate = Path::new(&home).join(".local/bin/ai-jail");
    candidate.is_file().then_some(candidate)
}

fn have_bwrap() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("bwrap").is_file()))
        .unwrap_or(false)
}

/// The real integration check: the argv we build is accepted by the installed
/// `ai-jail` under `--dry-run` (which prints the sandbox command without
/// executing), and the wrapped `ai-memory run claude --yolo` survives verbatim.
/// A malformed invocation — e.g. a value-taking `--agent-state` swallowing the
/// exe — fails here. Skips when ai-jail (or Linux `bwrap`) is absent.
#[test]
fn real_ai_jail_dry_run_accepts_and_forwards_the_invocation() {
    let Some(ai_jail) = ai_jail_path() else {
        eprintln!("skipping: ai-jail not installed");
        return;
    };
    if cfg!(target_os = "linux") && !have_bwrap() {
        eprintln!("skipping: bwrap not installed (Linux ai-jail backend)");
        return;
    }

    // Wrap a program that certainly exists, so any failure is about our argv
    // shape, not an unresolvable command.
    let exe = std::env::current_exe().expect("test binary path");
    let argv = build_ai_jail_invocation(&exe, &forwarded(), &["AI_MEMORY_SERVER_URL"], true);

    let output = Command::new(&ai_jail)
        .arg("--dry-run")
        .args(&argv)
        .output()
        .expect("run ai-jail --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert!(
        output.status.success(),
        "ai-jail --dry-run rejected the invocation:\nargv={argv:?}\nstdout={stdout}\nstderr={stderr}"
    );
    // The wrapped command must appear intact in the printed plan.
    for token in ["run", "claude", "--yolo"] {
        assert!(
            combined.contains(token),
            "dry-run plan is missing the wrapped token {token:?}; plan was:\n{combined}"
        );
    }
    assert!(
        combined.contains(&exe.to_string_lossy().into_owned()),
        "dry-run plan should name the wrapped ai-memory exe; plan was:\n{combined}"
    );
}
