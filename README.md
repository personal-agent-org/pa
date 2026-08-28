# Personal Agent TUI

`pa` is the terminal chat client for Personal Agent.

```bash
pa login --server https://pa.example.com
pa
pa logout
```

Its credentials and settings are stored only for the current user under
`~/.config/personal-agent/tui/`. They are never loaded from `/etc` and are not shared with the
desktop app or Computer Service.

The TUI consumes the chat API and does not expose tools, sensors, filesystem access, or other
host capabilities. Those are provided exclusively by the separate
[`computer-service`](https://github.com/personal-agent-org/computer-service), which uses its own
device-bound credential. The `/computer-service` command can install that service without giving
it access to the TUI's chat token.

## Build

```bash
cargo build --release
```

The `pa` binary is written to `target/release/pa`.

## Layout

- `crates/pa`: CLI entry point
- `crates/pa-tui`: terminal chat client
- `crates/pa-oidc`: device-flow authentication
