//! `ai-memory install-mcp` — print the MCP server registration
//! snippet for any supported client.
//!
//! The snippet format and the config-file location differ across
//! clients. We render the *content* the user needs to paste; we
//! deliberately do not auto-edit their config (formats are evolving
//! upstream and a bad merge is very user-visible).
//!
//! For clients that don't support remote MCP servers in their JSON
//! config (Claude Desktop today), the rendered snippet uses the
//! community-standard `npx mcp-remote` stdio shim so the same HTTP
//! endpoint still works.
//!
//! For clients that don't support MCP at all (`pi` per the upstream
//! author's stated position), we print an explanation + pointers
//! instead of fabricating a config.

use anyhow::Result;
use serde_json::json;

use crate::cli::{InstallMcpArgs, McpClient};
use crate::config::Config;

/// Run the `install-mcp` subcommand.
///
/// # Errors
/// Returns an error if JSON serialisation fails (should never happen
/// for our handcrafted values).
pub fn run(_config: &Config, args: InstallMcpArgs) -> Result<()> {
    let snippet = match args.client {
        McpClient::ClaudeCode => render_claude_code(&args)?,
        McpClient::Codex => render_codex(&args),
        McpClient::OpenCode => render_opencode(&args)?,
        McpClient::Cursor => render_cursor(&args)?,
        McpClient::ClaudeDesktop => render_claude_desktop(&args)?,
        McpClient::GeminiCli => render_gemini_cli(&args)?,
        McpClient::Openclaw => render_openclaw(&args)?,
        McpClient::Pi => render_pi_explanation(&args),
    };
    println!("{snippet}");
    Ok(())
}

fn render_claude_code(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Claude Code — register the MCP server\n\
         #\n\
         # Recommended (one-shot CLI):\n\
         claude mcp add --transport http {name} {url}\n\
         #\n\
         # Equivalent JSON if you'd rather edit settings directly\n\
         # (~/.claude/settings.json):\n\
         {snippet}\n",
        name = args.name,
        url = args.server_url,
        snippet = serde_json::to_string_pretty(&json!({
            "mcpServers": {
                args.name.as_str(): {
                    "type": "http",
                    "url": args.server_url,
                }
            }
        }))?,
    ))
}

fn render_codex(args: &InstallMcpArgs) -> String {
    // Codex uses TOML, not JSON. Hand-render the snippet so the
    // table headers stay deterministic.
    format!(
        "# Codex CLI — append to ~/.codex/config.toml\n\
         #\n\
         [mcp_servers.{name}]\n\
         url = \"{url}\"\n",
        name = args.name,
        url = args.server_url,
    )
}

fn render_opencode(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenCode — add to ~/.config/opencode/opencode.json under \"mcp\":\n\
         {snippet}\n",
        snippet = serde_json::to_string_pretty(&json!({
            "mcp": {
                args.name.as_str(): {
                    "type": "remote",
                    "url": args.server_url,
                    "enabled": true,
                }
            }
        }))?,
    ))
}

fn render_cursor(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Cursor — write to one of:\n\
         #   - ~/.cursor/mcp.json   (global, all projects)\n\
         #   - .cursor/mcp.json     (per-project, in the workspace root)\n\
         #\n\
         # Cursor supports HTTP MCP servers via the `url` field. Restart\n\
         # Cursor (or toggle the server off+on in Settings → MCP) after\n\
         # adding a new entry; live reload landed in recent builds but\n\
         # is still flaky.\n\
         {snippet}\n",
        snippet = serde_json::to_string_pretty(&json!({
            "mcpServers": {
                args.name.as_str(): {
                    "url": args.server_url,
                }
            }
        }))?,
    ))
}

fn render_claude_desktop(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Claude Desktop — write to claude_desktop_config.json:\n\
         #   - macOS:    ~/Library/Application Support/Claude/claude_desktop_config.json\n\
         #   - Windows:  %APPDATA%\\Claude\\claude_desktop_config.json\n\
         #   - Linux:    Claude Desktop is not officially distributed for Linux;\n\
         #               use Claude Code or another HTTP client instead.\n\
         #\n\
         # Claude Desktop's JSON config does not support HTTP MCP servers\n\
         # directly. We bridge through the community `mcp-remote` stdio shim\n\
         # (https://www.npmjs.com/package/mcp-remote). Requires Node.js.\n\
         # After editing, fully quit + relaunch Claude Desktop; \"Check for\n\
         # Updates\" is not enough.\n\
         {snippet}\n",
        snippet = serde_json::to_string_pretty(&json!({
            "mcpServers": {
                args.name.as_str(): {
                    "command": "npx",
                    "args": ["-y", "mcp-remote", args.server_url.as_str()],
                }
            }
        }))?,
    ))
}

fn render_gemini_cli(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# Gemini CLI — merge into ~/.gemini/settings.json:\n\
         #\n\
         # Gemini CLI uses `httpUrl` (not `url`) for streamable-HTTP\n\
         # endpoints. The `timeout` is in milliseconds.\n\
         {snippet}\n",
        snippet = serde_json::to_string_pretty(&json!({
            "mcpServers": {
                args.name.as_str(): {
                    "httpUrl": args.server_url,
                    "timeout": 5000,
                }
            }
        }))?,
    ))
}

fn render_openclaw(args: &InstallMcpArgs) -> Result<String> {
    Ok(format!(
        "# OpenClaw — merge into ~/.openclaw/config.json:\n\
         #\n\
         # OpenClaw distinguishes transports explicitly. Use\n\
         # \"transport\": \"streamable-http\" for ai-memory's HTTP endpoint.\n\
         {snippet}\n",
        snippet = serde_json::to_string_pretty(&json!({
            "mcp": {
                "servers": {
                    args.name.as_str(): {
                        "url": args.server_url,
                        "transport": "streamable-http",
                    }
                }
            }
        }))?,
    ))
}

fn render_pi_explanation(_args: &InstallMcpArgs) -> String {
    // No JSON to print. Plain-text explanation + pointers; mirrors
    // the docs/mcp-install.md#pi section so the help is self-contained.
    "# pi (@mariozechner/pi-coding-agent) — NOT supported via MCP\n\
     #\n\
     # pi's author has stated MCP support is not on the roadmap; see\n\
     # https://mariozechner.at/posts/2025-11-30-pi-coding-agent/ for the\n\
     # design rationale (token-budget concerns). agentmemory itself uses\n\
     # pi's native extension surface instead of MCP.\n\
     #\n\
     # Options if you want ai-memory + pi:\n\
     #   1. Run ai-memory and use it from another client (Claude Code,\n\
     #      Codex, Cursor, Gemini CLI) for the cross-agent handoff;\n\
     #      keep pi for code-edit sessions you don't need to persist.\n\
     #   2. Use the third-party `pi-mcp-adapter` shim\n\
     #      (https://github.com/nicobailon/pi-mcp-adapter) — unofficial,\n\
     #      community-maintained, may break on pi updates.\n\
     #\n\
     # See docs/mcp-install.md#pi-not-supported for the full discussion.\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(client: McpClient) -> InstallMcpArgs {
        InstallMcpArgs {
            client,
            server_url: "http://127.0.0.1:49374/mcp".into(),
            name: "ai-memory".into(),
        }
    }

    /// Sanity: every supported client renders without error and the
    /// output mentions the configured server URL (or, for pi, the
    /// "not supported" verbiage).
    #[test]
    fn every_client_renders() {
        for client in [
            McpClient::ClaudeCode,
            McpClient::Codex,
            McpClient::OpenCode,
            McpClient::Cursor,
            McpClient::ClaudeDesktop,
            McpClient::GeminiCli,
            McpClient::Openclaw,
            McpClient::Pi,
        ] {
            let out = render_for_test(client);
            if matches!(client, McpClient::Pi) {
                assert!(
                    out.contains("NOT supported"),
                    "pi must explain non-support: {out}"
                );
            } else {
                assert!(
                    out.contains("http://127.0.0.1:49374/mcp"),
                    "client {client:?} did not include the server URL in output:\n{out}"
                );
            }
        }
    }

    fn render_for_test(client: McpClient) -> String {
        let args = args_for(client);
        match args.client {
            McpClient::ClaudeCode => render_claude_code(&args).unwrap(),
            McpClient::Codex => render_codex(&args),
            McpClient::OpenCode => render_opencode(&args).unwrap(),
            McpClient::Cursor => render_cursor(&args).unwrap(),
            McpClient::ClaudeDesktop => render_claude_desktop(&args).unwrap(),
            McpClient::GeminiCli => render_gemini_cli(&args).unwrap(),
            McpClient::Openclaw => render_openclaw(&args).unwrap(),
            McpClient::Pi => render_pi_explanation(&args),
        }
    }

    /// Specific shape checks — each client has a distinguishing key
    /// in its JSON snippet. This catches accidental cross-pollination
    /// between renderers (e.g. Gemini's `httpUrl` showing up under
    /// Cursor's `mcpServers`).
    #[test]
    fn client_specific_shape_keys() {
        assert!(render_for_test(McpClient::Cursor).contains("\"url\""));
        assert!(render_for_test(McpClient::GeminiCli).contains("\"httpUrl\""));
        assert!(render_for_test(McpClient::ClaudeDesktop).contains("mcp-remote"));
        assert!(render_for_test(McpClient::Openclaw).contains("\"streamable-http\""));
        assert!(render_for_test(McpClient::Codex).contains("[mcp_servers.ai-memory]"));
    }
}
