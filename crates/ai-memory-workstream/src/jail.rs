//! ai-jail detection and invocation assembly for `ai-memory run --yolo`.
//!
//! See `docs/design-yolo-safety-ai-jail.md`. Detection and argv assembly are
//! pure and dependency-injected so every OS branch and the exact argv shape
//! are unit-tested without a real sandbox or a real PATH.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable names forwarded into ai-jail with `--env NAME`, when
/// actually set in the current process environment. ai-jail clears the
/// environment by default and re-adds only what is explicitly named, so a
/// managed run's server/hook URL and the harness's own credentials would
/// otherwise be invisible inside the jail.
pub const FORWARDED_ENV_NAMES: &[&str] = &[
    "AI_MEMORY_SERVER_URL",
    "AI_MEMORY_HOOK_URL",
    "AI_MEMORY_DATA_DIR",
    "AI_MEMORY_AUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// Whether `ai-jail` resolves through a `which`-style lookup. `lookup` is
/// injected so the resolution logic (PATH, `~/.local/bin`) is exercised by
/// [`ai_jail_on_path`] while this stays a pure predicate for tests.
#[must_use]
pub fn ai_jail_installed(lookup: impl Fn(&str) -> Option<PathBuf>) -> bool {
    lookup("ai-jail").is_some()
}

/// Real `ai-jail` lookup: `PATH`, falling back to `~/.local/bin/ai-jail`
/// (ai-jail's own documented install location when it is not on `PATH`).
#[must_use]
pub fn ai_jail_on_path() -> bool {
    ai_jail_installed(resolve_ai_jail)
}

fn resolve_ai_jail(name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home).join(".local").join("bin").join(name);
    is_executable_file(&candidate).then_some(candidate)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The operating system whose ai-jail "already inside" signal applies.
/// Taken explicitly (rather than read from `cfg!`) so [`inside_ai_jail`]'s
/// branches are all reachable from a single-platform test run; the real
/// caller uses [`current_jail_os`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailOs {
    /// Linux: ai-jail (bwrap) always sets the UTS hostname to `ai-sandbox`.
    Linux,
    /// macOS: ai-jail (seatbelt) has no UTS namespace, so it forces `PS1` to
    /// begin with `(jail) ` instead.
    MacOs,
    /// Windows: ai-jail is unsupported; never reports as jailed.
    Windows,
}

/// The host's actual OS, for the real (non-test) detection path.
#[must_use]
pub const fn current_jail_os() -> JailOs {
    if cfg!(target_os = "linux") {
        JailOs::Linux
    } else if cfg!(target_os = "macos") {
        JailOs::MacOs
    } else {
        JailOs::Windows
    }
}

/// Injected signals [`inside_ai_jail`] reads instead of the real process
/// environment, so detection is testable without a sandbox.
#[derive(Debug, Clone, Default)]
pub struct JailEnv {
    /// The current UTS hostname (Linux signal), e.g. from
    /// `/proc/sys/kernel/hostname`.
    pub hostname: Option<String>,
    /// The current `PS1` value (macOS signal).
    pub ps1: Option<String>,
}

/// Whether the process is already running inside ai-jail, per
/// `docs/design-yolo-safety-ai-jail.md` §3.
///
/// Fails open to the safe side: an unrecognized or missing signal returns
/// `false` (not jailed), so the yolo warning is shown rather than silently
/// skipped. A false positive is not plausible for either signal by
/// construction (ai-memory execs directly, not through an interactive
/// shell); a false negative just repeats the warning inside a real jail,
/// which is safe.
#[must_use]
pub fn inside_ai_jail(env: &JailEnv, os: JailOs) -> bool {
    match os {
        JailOs::Linux => env.hostname.as_deref() == Some("ai-sandbox"),
        JailOs::MacOs => env
            .ps1
            .as_deref()
            .is_some_and(|ps1| ps1.starts_with("(jail) ")),
        JailOs::Windows => false,
    }
}

/// Real "already inside ai-jail" check: reads the Linux hostname file and the
/// process's own `PS1`, then applies [`inside_ai_jail`] for [`current_jail_os`].
#[must_use]
pub fn inside_ai_jail_here() -> bool {
    let env = JailEnv {
        hostname: read_linux_hostname(),
        ps1: std::env::var("PS1").ok(),
    };
    inside_ai_jail(&env, current_jail_os())
}

fn read_linux_hostname() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
}

/// Build the argument vector for `ai-jail` (excluding the `ai-jail` program
/// name itself): `--network`, an optional bare `--agent-state` toggle, one
/// `--env NAME` per already-filtered present name, then the wrapped
/// executable and its forwarded arguments in order.
///
/// `--agent-state` is a boolean toggle in ai-jail (`--agent-state` /
/// `--no-agent-state`), not a valued flag — it persists the harness's own
/// credential state across ai-jail's otherwise-ephemeral private home; ai-jail
/// derives the per-harness state location itself from the wrapped
/// `ai-memory run <harness>` it parses. `env_names_present` is caller-filtered
/// (only names actually set in the current environment), keeping this function
/// pure and independent of the real process environment.
#[must_use]
pub fn build_ai_jail_invocation(
    exe: &Path,
    forwarded_args: &[OsString],
    env_names_present: &[&str],
    agent_state: bool,
) -> Vec<OsString> {
    let mut argv = vec![OsString::from("--network")];
    if agent_state {
        argv.push(OsString::from("--agent-state"));
    }
    for name in env_names_present {
        argv.push(OsString::from("--env"));
        argv.push(OsString::from(*name));
    }
    argv.push(exe.as_os_str().to_os_string());
    argv.extend(forwarded_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn ai_jail_installed_true_when_lookup_resolves() {
        assert!(ai_jail_installed(|name| {
            assert_eq!(name, "ai-jail");
            Some(PathBuf::from("/usr/bin/ai-jail"))
        }));
    }

    #[test]
    fn ai_jail_installed_false_when_lookup_misses() {
        assert!(!ai_jail_installed(|_| None));
    }

    #[test]
    fn inside_ai_jail_linux_matches_sandbox_hostname() {
        let jailed = JailEnv {
            hostname: Some("ai-sandbox".to_string()),
            ps1: None,
        };
        assert!(inside_ai_jail(&jailed, JailOs::Linux));

        let not_jailed = JailEnv {
            hostname: Some("dev-box".to_string()),
            ps1: None,
        };
        assert!(!inside_ai_jail(&not_jailed, JailOs::Linux));

        let unknown = JailEnv::default();
        assert!(!inside_ai_jail(&unknown, JailOs::Linux));
    }

    #[test]
    fn inside_ai_jail_macos_matches_ps1_prefix() {
        let jailed = JailEnv {
            hostname: None,
            ps1: Some("(jail) user@host $ ".to_string()),
        };
        assert!(inside_ai_jail(&jailed, JailOs::MacOs));

        let not_jailed = JailEnv {
            hostname: None,
            ps1: Some("user@host $ ".to_string()),
        };
        assert!(!inside_ai_jail(&not_jailed, JailOs::MacOs));

        let unknown = JailEnv::default();
        assert!(!inside_ai_jail(&unknown, JailOs::MacOs));
    }

    #[test]
    fn inside_ai_jail_windows_always_false() {
        let env = JailEnv {
            hostname: Some("ai-sandbox".to_string()),
            ps1: Some("(jail) ".to_string()),
        };
        assert!(!inside_ai_jail(&env, JailOs::Windows));
    }

    #[test]
    fn build_ai_jail_invocation_assembles_network_env_and_argv_in_order() {
        let exe = Path::new("/usr/local/bin/ai-memory");
        let forwarded = vec![
            OsString::from("run"),
            OsString::from("claude"),
            OsString::from("--yolo"),
        ];
        let present = ["AI_MEMORY_SERVER_URL", "ANTHROPIC_API_KEY"];
        let argv = build_ai_jail_invocation(exe, &forwarded, &present, true);
        assert_eq!(
            strings(&argv),
            [
                "--network",
                "--agent-state",
                "--env",
                "AI_MEMORY_SERVER_URL",
                "--env",
                "ANTHROPIC_API_KEY",
                "/usr/local/bin/ai-memory",
                "run",
                "claude",
                "--yolo",
            ]
        );
    }

    #[test]
    fn build_ai_jail_invocation_omits_agent_state_when_none() {
        let exe = Path::new("/usr/local/bin/ai-memory");
        let argv = build_ai_jail_invocation(exe, &[], &[], false);
        assert_eq!(strings(&argv), ["--network", "/usr/local/bin/ai-memory"]);
    }

    #[test]
    fn build_ai_jail_invocation_only_forwards_present_env_names() {
        let exe = Path::new("/bin/ai-memory");
        let argv = build_ai_jail_invocation(exe, &[], &["CLAUDE_CONFIG_DIR"], false);
        assert_eq!(
            strings(&argv),
            ["--network", "--env", "CLAUDE_CONFIG_DIR", "/bin/ai-memory"]
        );
    }
}
