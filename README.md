# pa - Personal Agent CLI

One binary, three faces:

```
pa                      # terminal chat UI (default)
pa login --server … --issuer …
pa logout
pa gui                  # desktop window (gui-enabled build only)
pa service enroll --server … --device … --issuer …
pa service start        # connect this machine + serve coding tools (alias: run)
pa service tools        # print the coding-tool catalog (JSON)
```

## Builds

Two flavours from one source:

- **headless** (`cargo build --release`, or `just build`): terminal UI + device service.
  Zero webview dependency, runs on headless servers. This is the `pa` shipped by the
  one-liner install and served to TUI users.
- **desktop** (`cargo build --release --features gui`, or `just build-gui`): the same `pa`
  plus the Tauri GUI, so `pa gui` opens the app window. Needs a platform webview at runtime
  (webkit2gtk on Linux, WebView2 on Windows, WKWebView on macOS). This is the AppImage/.deb
  distribution.

`pa gui` on a headless build prints a hint and exits; the GUI is compiled in only with
`--features gui`, so servers never drag in the webview.

## Layout

`crates/pa` (bin, clap dispatch) depends on `crates/pa-tui` and `crates/pa-agent`
(libraries); `crates/pa-gui` (the Tauri app) is linked only behind the `gui` feature.
