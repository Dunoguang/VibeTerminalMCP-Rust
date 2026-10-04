# VibeTerminalMCP-Rust

A high-performance, memory-efficient Model Context Protocol (MCP) server providing persistent interactive pseudo-terminal (PTY) sessions, rewritten in Rust using **Tokio** and **Axum**.

Successor to the C++ [VibeTerminalMCP](https://github.com/Dunoguang/VibeTerminalMCP).

## Features

- **Protocol**: MCP 2025-06-18 Streamable HTTP (`/mcp`), with backwards compatibility for `/sse`, `/message`, `/messages`, and root endpoints.
- **PTY Management**: Full interactive bash shell session lifecycle (`forkpty`, dynamic window resize, process cleanup).
- **Exact Signal Dispatching**: Bypasses tty ldisc limitations to dispatch signals (`SIGINT`, `SIGTSTP`, `SIGQUIT`) directly to the foreground process group (`/proc/<pid>/stat` `tpgid`).
- **Reliable Command Completion**: Marker-based detection (`wait_marker`) scanning outputs across chunk boundaries with exit-code assertion, plus fallback prompt detection.
- **UTF-8 & Non-ASCII Safe**: Full native multi-byte character preservation (no bit-stripping, safe ANSI stripping).
- **Mandatory Auditing**: Every tool call enforces an operation reason, appending to a structured `audit.log`.
- **Lightweight**: Standalone ~2.2MB binary, low idle RSS.

## Tools (10)

| Tool | Description |
|---|---|
| `terminal_new` | Spawn a persistent PTY bash session with custom dimensions/env/cwd. |
| `terminal_add_cmd` | Send string/keystrokes to the terminal session (with optional auto-enter). |
| `terminal_wait` | Wait for command completion (marker mode with exit code, or prompt mode). |
| `terminal_line` | Slice terminal output by line range (1-based, 4KB capped). |
| `terminal_last` | Retrieve new output since last query (4KB capped). |
| `terminal_timeout` | Read output after waiting `n` milliseconds. |
| `terminal_kill` | Terminate and clean up a session. |
| `terminal_list` | List active sessions, idle times, and recent output. |
| `terminal_resize` | Dynamically adjust terminal window rows and cols. |
| `get_tools` | Introspect tool definitions and JSON Schemas. |

## Build & Run

### Prerequisites

- Rust 1.75+ (Cargo)

### Build Release

```bash
cargo build --release
```

The compiled binary will be placed at `target/release/vibeterminalmcp-server`.

### Run

```bash
./target/release/vibeterminalmcp-server --host 127.0.0.1 --port 8001
```

Options:
- `--host HOST`: Bind address (default `127.0.0.1`).
- `--port PORT`: Port (default `8001`).
- `--audit-log PATH`: Custom audit log file location (default: `audit.log` or env `SHELL_MCP_AUDIT_LOG`).

## License

[MIT](LICENSE) © 2026 Dunoguang
