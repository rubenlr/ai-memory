# Design: safer `--yolo` and ai-jail integration

Status: accepted (release/2.5). Tracks the 2.5 feature that makes
`ai-memory run … --yolo` warn before it disarms an agent's safety prompts,
offers to run the session inside [ai-jail](https://github.com/akitaonrails/ai-jail)
when it is installed, and adds an opt-in "true yolo" for Claude Code that
silences the residual permission pauses `--dangerously-skip-permissions`
leaves behind.

## Motivation

`--yolo` maps to each harness's dangerous-mode flag (`apply_yolo`;
Claude → `--dangerously-skip-permissions`). That is a loaded footgun: it runs
every tool call with no confirmation. Three gaps:

1. **No warning.** A user types `--yolo` and the agent is immediately
   unsupervised, with no reminder of what that means or a way to back out.
2. **No sandbox nudge.** ai-jail exists precisely to contain an unsupervised
   agent, but nothing connects the two — the user must remember to type
   `ai-jail ai-memory run …` themselves.
3. **Claude still pauses.** Even with `--dangerously-skip-permissions`, Claude
   Code still prompts on `permissions.ask`/`deny` rules and on critical-path
   `rm` (a 2-minute timeout prompt), so an "unattended" yolo run stalls.

## Non-goals

- Changing default (non-`--yolo`) behavior. Everything here is gated on
  `--yolo`.
- Modifying ai-jail. Detection uses ai-jail's *existing* observable surface
  (see "Already inside ai-jail"). No companion change is required.
- Prompting in any non-interactive path. The warning/offer only appear on a
  real TTY (hook, CI, detached, and piped runs are untouched).

## The four parts

### 1. Yolo warning prompt (all OSes)

Before `apply_yolo` runs in `run.rs`, when `--yolo` is requested **and** the
session is interactive (`stdin` and `stderr` are both terminals — the same
gate the native-session picker already uses) **and** we are not already inside
ai-jail:

```
⚠  --yolo runs every tool call with no confirmation. An agent can delete
   files, run any command, and reach the network unsupervised.
   Proceed? [Y/n]
```

Short, glanceable, `Enter` = yes. A `n`/`no` aborts before the agent spawns
(exit non-zero, nothing launched). EOF/empty ⇒ yes (the default), because a
bare `Enter` is the common case. This never fires in a non-TTY path, so
scripts, hooks, and CI keep working unchanged.

### 2. ai-jail detect + offer (Linux/macOS)

If `ai-jail` is on `PATH` (`command -v ai-jail`, fallback `~/.local/bin/ai-jail`)
and we are not already jailed, the prompt gains a second question:

```
ai-jail is installed. Re-run this session inside it? [Y/n]
```

On yes, ai-memory re-execs itself under ai-jail instead of spawning the agent
directly:

```
ai-jail --network --agent-state <state> \
        --env AI_MEMORY_SERVER_URL --env AI_MEMORY_HOOK_URL \
        --env ANTHROPIC_API_KEY --env CLAUDE_CODE_OAUTH_TOKEN \
        --env CLAUDE_CONFIG_DIR --env … \
        <current_exe> run <harness> … --yolo
```

- **Re-exec**, not a nested spawn: `std::env::current_exe()` + the original
  `args_os()`. ai-jail forwards the wrapped argv verbatim and already parses
  `ai-memory run <harness>` (it keeps both the `ai-memory` binary and the
  native harness visible under its private home), so the nested launch works.
- **`--network` (not `--allow-host`).** ai-jail only adds `--unshare-net` when
  network is *off* (`bwrap.rs`: `if !network_enabled { push("--unshare-net") }`),
  so `--network` shares the host network namespace and the host-side ai-memory
  server on `127.0.0.1:49374` — which the managed run and the agent's hooks
  both need — is reachable. Filtered `--allow-host` uses a private netns with a
  CONNECT proxy for *external* hosts only, so it would cut the loopback server
  off; we deliberately do not use it. This is soft protection (full egress),
  which is acceptable: the agent needs outbound network for its own model API
  regardless, and an agent with no access is useless.
- **Env passthrough.** ai-jail clears the environment by default and re-adds a
  minimal allowlist, so `AI_MEMORY_*`, `CLAUDE_CONFIG_DIR`, and provider tokens
  are **not** inherited. ai-memory forwards the ones it set (server/hook URL,
  the token vars it already threads) with `--env NAME`. Only names that are
  actually set in the current environment are forwarded.
- **`--agent-state`** so the harness's own login/credentials persist across the
  private home.

If the user declines the ai-jail offer but confirmed `--yolo`, the run
proceeds unjailed (their choice, already warned).

### 3. Already inside ai-jail (skip the warning, avoid double-wrap)

ai-jail exports **no** dedicated sentinel env var, and we chose not to add one
(keeps the two projects decoupled). Detection uses ai-jail's existing
observable surface, per OS:

- **Linux:** ai-jail always sets a fresh UTS namespace with hostname
  `ai-sandbox` (`bwrap.rs`, unconditional). Read `/proc/sys/kernel/hostname`
  (or `uname`/`gethostname`) and compare. Reliable.
- **macOS:** seatbelt has no UTS namespace, so the hostname is unchanged.
  ai-jail forces `PS1` to begin with `(jail) ` on both backends; on macOS that
  is the available signal. Weaker (a shell can overwrite `PS1`), but it only
  ever *suppresses* the warning — a false negative just shows the warning
  again inside the jail, which is safe; a false positive is unlikely because
  ai-memory is exec'd directly by ai-jail, not through an interactive shell
  that would reset `PS1`.
- **Windows:** ai-jail is unsupported, so this always returns "not jailed";
  the ai-jail offer is never shown.

When detected as already jailed, both the warning and the ai-jail offer are
skipped and the run proceeds directly — a user who typed
`ai-jail ai-memory run … --yolo` sees no extra friction.

### 4. Claude "true yolo" (opt-in, all OSes; recommended only under ai-jail)

`--dangerously-skip-permissions` alone still pauses. Opt-in
`[run] claude_true_yolo` (config) / `--true-yolo` (flag) additionally, **for
the Claude harness only**:

- Sets `CLAUDE_CODE_DISABLE_DANGEROUS_RM_TIMEOUT=1`,
  `CLAUDE_CODE_DISABLE_SUBSTITUTION_RM_PROMPT=1`, and
  `CLAUDE_CODE_DISABLE_POWERSHELL_CMD_RM_DENY=1` in the child env (the last is a
  no-op off Windows; harmless to set everywhere). These remove the residual
  `rm` prompts.
- Injects `--settings '{"permissions":{"defaultMode":"bypassPermissions","ask":[]}}'`
  on the Claude argv (CLI-flag precedence sits above user settings). This does
  not remove a user's own `deny`/`ask` rules (those union across levels), so
  true-yolo is documented as "best paired with a clean sandbox," i.e. ai-jail.

Off by default. When enabled without ai-jail (and interactive), the warning
text says so. Only applies to `ManagedHarness::Claude`; a no-op for other
harnesses (documented, not silently ignored — a one-line note if `--true-yolo`
is passed with a non-Claude harness).

## OS support matrix

| Concern | Linux | macOS | Windows |
|---|---|---|---|
| ai-jail available | bwrap | seatbelt | unsupported → no offer |
| already-jailed detect | hostname `ai-sandbox` | `PS1` `(jail) ` | always "no" |
| `--network` loopback reach | shared net ns | profile-allowed | n/a |
| warning prompt | ✓ | ✓ | ✓ |
| true-yolo env vars | ✓ | ✓ | ✓ (incl. powershell var) |

## Code shape

- `ai-memory-workstream/src/jail.rs` (new): pure, OS-aware, dependency-injected
  detection + command construction — `ai_jail_installed(lookup)`,
  `inside_ai_jail(env, hostname)`, `build_ai_jail_invocation(exe, args, env_names)`.
  Pure functions so the OS branches and the argv/env assembly are unit-tested
  without a sandbox.
- `ai-memory-cli/src/commands/run.rs`: the interactive prompt + orchestration
  (gate → warn → offer → re-exec or proceed), reusing the existing
  `is_terminal` gate and a `confirm`-style reader.
- `ai-memory-cli/src/config.rs`: `[run] claude_true_yolo: bool` (default false).
- `ai-memory-workstream/src/harness.rs`: `apply_claude_true_yolo(env, args)`
  next to `apply_yolo`.

## Security considerations

- The warning/offer are **TTY-gated**; no new prompt reaches hook, CI,
  detached, or piped paths (mirrors the interactive-lock and native-picker
  gates).
- Re-exec forwards only environment names that are already set; it introduces
  no new secret surface and does not log token values.
- true-yolo is off by default, Claude-only, and documented as sandbox-first.
- Detection failure fails *open* to the safe side: if we cannot tell we are
  jailed, we show the warning (never silently skip it).

## Testing

- `jail.rs` unit tests: installed/not (injected lookup), inside/outside per OS
  (injected env + hostname), invocation assembly (flags, `--env` only for set
  names, current-exe + argv forwarding).
- `run.rs`: prompt gate is off without a TTY; decline aborts before launch;
  already-jailed skips the prompt; the ai-jail offer is absent when ai-jail is
  not found.
- `harness.rs`: `apply_claude_true_yolo` sets the three env vars + the
  `--settings` arg for Claude and is a no-op for other harnesses.
- CHANGELOG `### Added` (minor); support-matrix + cookbook updated in the same
  change.
