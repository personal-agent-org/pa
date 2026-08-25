# `pa` — Personal Agent chat clients

One repository and one binary for the terminal and desktop chat experiences:

```bash
pa                      # terminal chat UI (default)
pa login --server …     # terminal login
pa logout
pa gui                  # desktop window (GUI-enabled build)
```

Desktop/TUI configuration is stored only per user under
`~/.config/personal-agent/desktop/`. It is never loaded from `/etc`.

Both surfaces are clients of the same HTTP/SSE/control-WebSocket API. They do not announce tools,
sensors, filesystem access, or other host functions to the backend. Computer capabilities are
provided exclusively by the separate
[`computer-service`](https://github.com/personal-agent-org/computer-service).

Both clients offer installation of that separate background service as the `pacs` command: the
desktop from Settings, the TUI through `/computer-service [device name]`. The service runs as its
own process with its own device-bound credential. Desktop/TUI chat tokens are never shared with it.

## Builds

- **Terminal:** `cargo build --release` or `just build`. No webview dependency.
- **Desktop:** `cargo build --release --features gui` or `just build-gui`. Uses Tauri and the
  platform webview.

## Layout

- `crates/pa`: binary and command dispatch
- `crates/pa-tui`: terminal chat client
- `crates/pa-gui`: desktop chat shell and local Computer Service management
- `crates/pa-oidc`: chat-client device-flow authentication
