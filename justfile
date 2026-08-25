# Personal Agent chat clients (`pa`): TUI by default, desktop with `pa gui`.

# Terminal build (no webview dependency).
build:
    cargo build --release

# Desktop build (adds the Tauri GUI behind --features gui). Needs webkit2gtk + gtk3 dev libs.
build-gui:
    cargo build --release --features gui

fmt:
    cargo fmt

lint:
    cargo clippy --workspace --all-targets

check: fmt lint
    cargo build --release
    cargo build --release --features gui

# Regenerate the GUI icon set from app-icon.svg (needs rsvg-convert).
icons:
    cd crates/pa-gui && for s in 32 128 256 512; do rsvg-convert -w "$s" -h "$s" app-icon.svg -o "icons/${s}x${s}.png"; done
    cd crates/pa-gui && cp icons/256x256.png icons/128x128@2x.png && cp icons/512x512.png icons/icon.png
