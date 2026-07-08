fn main() {
    // Tauri codegen (reads tauri.conf.json, generates the context + permission schemas) is
    // only needed for the gui-enabled build; the headless pa must build without it.
    if std::env::var_os("CARGO_FEATURE_GUI").is_some() {
        tauri_build::build();
    }
}
