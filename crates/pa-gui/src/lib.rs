// The Personal Agent desktop shell: a native WebKitGTK window that loads the Personal Agent
// instance you point it at. On first launch a small local setup screen asks for the server
// URL, which is persisted and loaded next time (changeable later from the tray).
// Beyond the window it adds:
//   - single-instance (a 2nd launch focuses the running window),
//   - window-state persistence (size/position across launches),
//   - an in-window navigation allowlist (the SPA + its same-domain Keycloak stay in-window;
//     everything else opens in the system browser),
//   - a system tray + close-to-tray (the SPA keeps running in the background so its
//     control-WS push events still arrive), with a "change server" action,
//   - a NARROW native bridge (`window.personalAgentNative`) the SPA already knows how to talk
//     to: it surfaces background pushes (nudge/draft/approval/question) as OS notifications and
//     opens external links natively. The bridge sets `ownsAuth:false` so the SPA keeps its
//     normal in-window Keycloak web login.
//
// Security: the bridge's single `native_bridge` command has a fixed vocabulary (no fs/shell/
// exec). The IPC remote.urls allowlist is a wildcard because the server is runtime-chosen, but
// the `on_navigation` allowlist below is the real guard: only the chosen instance + its
// same-domain Keycloak ever load in the webview, so only those origins can reach the bridge.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_window_state::{StateFlags, WindowExt};

// The JS the SPA's nativeBridge.ts looks for. Runs before the page's own scripts on every
// load (incl. the OIDC redirect back), so `isNative()` is true from the first tick.
// `ownsAuth:false` keeps the SPA on its in-window Keycloak web flow (we do NOT own auth).
const BRIDGE_INIT_JS: &str = r#"
;(function () {
  if (window.personalAgentNative) return;
  window.personalAgentNative = {
    ownsAuth: false,
    platform: 'desktop',
    postMessage: function (json) {
      try { window.__TAURI__.core.invoke('native_bridge', { message: json }); } catch (e) {}
    }
  };
})();
"#;

// Where the chosen server URL is persisted (per-user app config dir).
fn server_file(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("server-url"))
}

fn read_server(app: &tauri::AppHandle) -> Option<String> {
    let s = fs::read_to_string(server_file(app)?).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// UI strings, embedded at compile time from the SAME ui/locales/<lang>.json files the setup
// screen fetches and Weblate manages, so the tray + error messages stay in sync with the
// setup screen. Picks German for a de* system locale, English otherwise.
fn locale_map() -> serde_json::Map<String, Value> {
    let lang = sys_locale::get_locale().unwrap_or_default().to_lowercase();
    let raw = if lang.starts_with("de") {
        include_str!("../../pa/ui/locales/de.json")
    } else {
        include_str!("../../pa/ui/locales/en.json")
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn tr(map: &serde_json::Map<String, Value>, key: &str, fallback: &str) -> String {
    map.get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

// The registrable-ish domain = the last two dot labels (pa.example.com -> example.com), so the
// app + a same-domain Keycloak (id.example.com) stay in-window. Not eTLD-aware; fine here.
fn base_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() >= 2 {
        parts[parts.len() - 2..].join(".")
    } else {
        host.to_string()
    }
}

#[tauri::command]
fn get_server(app: tauri::AppHandle) -> Option<String> {
    read_server(&app)
}

// Persist the chosen server URL, then relaunch so the main window opens on it.
#[tauri::command]
fn set_server(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let t = locale_map();
    let url = url.trim().trim_end_matches('/').to_string();
    let parsed = tauri::Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(tr(
            &t,
            "error_scheme",
            "URL must start with http:// or https://",
        ));
    }
    if parsed.host_str().is_none() {
        return Err(tr(&t, "error_host", "URL has no host"));
    }
    let path = server_file(&app).ok_or("no config directory")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(&path, &url).map_err(|e| e.to_string())?;
    app.restart()
}

// Forget the saved server and relaunch back into the setup screen.
fn forget_and_restart(app: &tauri::AppHandle) -> ! {
    if let Some(p) = server_file(app) {
        let _ = fs::remove_file(p);
    }
    app.restart()
}

#[tauri::command]
fn forget_server(app: tauri::AppHandle) {
    forget_and_restart(&app)
}

// Echo a request/response reply back into the SPA via the callback it installed.
fn reply(
    window: &tauri::WebviewWindow,
    id: Option<i64>,
    success: bool,
    payload: Value,
    error: &str,
) {
    let Some(id) = id else { return }; // fire-and-forget command -> nothing to answer
    let mut obj = json!({ "id": id, "type": "response", "success": success });
    if !payload.is_null() {
        obj["payload"] = payload;
    }
    if !error.is_empty() {
        obj["error"] = json!(error);
    }
    let js = format!(
        "window.personalAgentNativeCallback && window.personalAgentNativeCallback({})",
        obj
    );
    let _ = window.eval(&js);
}

// --- Device-agent: connect THIS computer as a device (download the agent, enroll via the OIDC
// device flow, run it as a systemd user service) + configure which tools it exposes. ---

fn agent_bin_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".local")
            .join("bin")
            .join("personal-agent-device-agent")
    })
}

fn agent_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("personal-agent-device").join("config.toml"))
}

fn default_workspace() -> String {
    dirs::home_dir()
        .map(|h| h.join("projects").to_string_lossy().into_owned())
        .unwrap_or_else(|| "projects".into())
}

// The release-asset slug the backend serves the binary under (linux is x64-only; macOS arm64).
fn agent_platform_slug() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-x64"
    } else if cfg!(target_os = "macos") {
        "macos-arm64"
    } else {
        "linux-x64"
    }
}

// Push a one-line progress update to the SPA (the desktop settings show the latest line).
fn emit_progress(window: &tauri::WebviewWindow, msg: &str) {
    let obj = json!({ "type": "device-agent/progress", "payload": { "message": msg } });
    let js = format!(
        "window.personalAgentNativeCallback && window.personalAgentNativeCallback({})",
        obj
    );
    let _ = window.eval(&js);
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| {
        d.join("systemd")
            .join("user")
            .join("personal-agent-device-agent.service")
    })
}

#[cfg(target_os = "linux")]
fn service_active() -> bool {
    std::process::Command::new("systemctl")
        .args([
            "--user",
            "is-active",
            "--quiet",
            "personal-agent-device-agent.service",
        ])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(target_os = "linux"))]
fn service_active() -> bool {
    false
}

fn device_agent_status() -> Value {
    let installed = agent_bin_path().is_some_and(|p| p.exists());
    let enrolled = agent_config_path().is_some_and(|p| p.exists());
    json!({ "installed": installed, "enrolled": enrolled, "running": service_active() })
}

// Read the tool-exposure flags the agent honours from its config.toml (defaults when absent).
fn read_agent_flags() -> (Vec<String>, bool) {
    let Some(path) = agent_config_path() else {
        return (Vec::new(), true);
    };
    let Ok(txt) = std::fs::read_to_string(&path) else {
        return (Vec::new(), true);
    };
    let Ok(val) = txt.parse::<toml::Value>() else {
        return (Vec::new(), true);
    };
    let disabled = val
        .get("disabled_tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let home = val
        .get("expose_home_index")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    (disabled, home)
}

// The available tool catalog (from `<bin> tools`) + the current exposure flags, so the desktop
// settings can render per-tool toggles.
fn device_agent_config() -> Value {
    let tools = agent_bin_path()
        .filter(|p| p.exists())
        .and_then(|b| {
            std::process::Command::new(&b)
                .arg("tools")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        })
        .unwrap_or_else(|| json!([]));
    let (disabled, home) = read_agent_flags();
    json!({ "tools": tools, "disabledTools": disabled, "exposeHomeIndex": home })
}

// Persist the exposure flags into the agent config.toml, then restart the service so the new
// hello announcement (with the narrowed tool set) takes effect.
fn set_device_agent_config(disabled: Vec<String>, expose_home: bool) -> Result<(), String> {
    let path = agent_config_path().ok_or("no config directory")?;
    let txt = std::fs::read_to_string(&path).map_err(|_| "agent not enrolled".to_string())?;
    let mut val: toml::Value = txt.parse().map_err(|e| format!("config parse: {e}"))?;
    if let Some(tbl) = val.as_table_mut() {
        tbl.insert(
            "disabled_tools".into(),
            toml::Value::Array(disabled.into_iter().map(toml::Value::String).collect()),
        );
        tbl.insert(
            "expose_home_index".into(),
            toml::Value::Boolean(expose_home),
        );
    }
    let out = toml::to_string(&val).map_err(|e| e.to_string())?;
    std::fs::write(&path, out).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    restart_service();
    Ok(())
}

#[cfg(target_os = "linux")]
fn restart_service() {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "restart", "personal-agent-device-agent.service"])
        .status();
}

#[cfg(not(target_os = "linux"))]
fn restart_service() {}

fn download_agent(server: &str, dest: &Path, window: &tauri::WebviewWindow) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/devices/agent/bin/{}",
        server.trim_end_matches('/'),
        agent_platform_slug()
    );
    emit_progress(window, &format!("Lade Agent ({})…", agent_platform_slug()));
    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("Download fehlgeschlagen: {e}"))?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// Run the agent's OIDC device-flow enrollment, surfacing each status line and opening the
// verification URL it prints (so the user just approves in the browser).
fn enroll_agent(
    bin: &Path,
    server: &str,
    device: &str,
    issuer: &str,
    client: &str,
    workspace: &str,
    window: &tauri::WebviewWindow,
) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};
    emit_progress(window, "Anmeldung (im Browser bestätigen)…");
    let mut child = Command::new(bin)
        .args([
            "enroll",
            "--server",
            server,
            "--device",
            device,
            "--issuer",
            issuer,
            "--client",
            client,
            "--workspace",
            workspace,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("enroll: {e}"))?;
    if let Some(err) = child.stderr.take() {
        let mut opened = false;
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            emit_progress(window, &line);
            if !opened {
                if let Some(u) = line.split_whitespace().find(|w| w.starts_with("http")) {
                    let _ = window
                        .app_handle()
                        .opener()
                        .open_url(u.to_string(), None::<&str>);
                    opened = true;
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Anmeldung fehlgeschlagen".into())
    }
}

#[cfg(target_os = "linux")]
fn enable_service(bin: &Path, window: &tauri::WebviewWindow) -> Result<(), String> {
    use std::process::Command;
    emit_progress(window, "Richte systemd-User-Service ein…");
    let unit = systemd_unit_path().ok_or("no config directory")?;
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let contents = format!(
        "[Unit]\n\
         Description=Personal Agent device agent\n\
         After=network-online.target\n\
         Wants=network-online.target\n\n\
         [Service]\n\
         ExecStart={} run\n\
         Restart=on-failure\n\
         RestartSec=5\n\n\
         [Install]\n\
         WantedBy=default.target\n",
        bin.display()
    );
    std::fs::write(&unit, contents).map_err(|e| e.to_string())?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    // Keep the service running across logouts (best-effort; may prompt for authorization).
    let _ = Command::new("loginctl").arg("enable-linger").status();
    let ok = Command::new("systemctl")
        .args([
            "--user",
            "enable",
            "--now",
            "personal-agent-device-agent.service",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err("Service konnte nicht gestartet werden".into())
    }
}

#[cfg(not(target_os = "linux"))]
fn enable_service(_bin: &Path, _window: &tauri::WebviewWindow) -> Result<(), String> {
    Err("Service-Einrichtung wird nur unter Linux unterstützt".into())
}

fn install_device_agent(
    window: &tauri::WebviewWindow,
    server: &str,
    device: &str,
    issuer: &str,
    client: &str,
    workspace: &str,
) -> Result<(), String> {
    let bin = agent_bin_path().ok_or("no home directory")?;
    download_agent(server, &bin, window)?;
    enroll_agent(&bin, server, device, issuer, client, workspace, window)?;
    enable_service(&bin, window)?;
    emit_progress(window, "Fertig ✓");
    Ok(())
}

// The SPA -> native bridge. Fixed vocabulary; unknown request-shaped messages are rejected
// so the SPA falls back to its web behavior (e.g. saveBlob -> <a download>).
#[tauri::command]
fn native_bridge(window: tauri::WebviewWindow, message: String) {
    let Ok(msg) = serde_json::from_str::<Value>(&message) else {
        return;
    };
    let typ = msg.get("type").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").and_then(Value::as_i64);
    let payload = msg.get("payload");
    let pstr = |key: &str| {
        payload
            .and_then(|p| p.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };

    match typ {
        "config/get" => {
            let version = window.app_handle().package_info().version.to_string();
            reply(
                &window,
                id,
                true,
                json!({
                    "hasMic": false,
                    "hasNotifications": true,
                    "hasWakeWord": false,
                    "hasDeviceSensors": false,
                    "pushType": "ws",
                    "appVersion": version
                }),
                "",
            );
        }
        "notifications/request-permission" => {
            reply(&window, id, true, json!({ "granted": true }), "");
        }
        "notification/show" => {
            let title = {
                let t = pstr("title");
                if t.is_empty() {
                    "Personal Agent".to_string()
                } else {
                    t
                }
            };
            let _ = window
                .app_handle()
                .notification()
                .builder()
                .title(title)
                .body(pstr("body"))
                .show();
        }
        "open-external" => {
            let url = pstr("url");
            if url.starts_with("http://") || url.starts_with("https://") {
                let _ = window.app_handle().opener().open_url(url, None::<&str>);
            }
        }
        "device-agent/status" => {
            reply(&window, id, true, device_agent_status(), "");
        }
        "device-agent/install" => {
            let server = pstr("server");
            let device = pstr("device");
            let issuer = pstr("issuer");
            let client = {
                let c = pstr("client");
                if c.is_empty() {
                    "personal-agent-device".to_string()
                } else {
                    c
                }
            };
            let workspace = {
                let w = pstr("workspace");
                if w.is_empty() {
                    default_workspace()
                } else {
                    w
                }
            };
            // Long-running + interactive (download + device-flow enroll + service): run off the
            // command thread and reply when done; progress streams as device-agent/progress events.
            let win = window.clone();
            std::thread::spawn(move || {
                match install_device_agent(&win, &server, &device, &issuer, &client, &workspace) {
                    Ok(()) => reply(&win, id, true, json!({ "ok": true }), ""),
                    Err(e) => {
                        emit_progress(&win, &e);
                        reply(&win, id, false, Value::Null, &e);
                    }
                }
            });
        }
        "device-agent/config-get" => {
            reply(&window, id, true, device_agent_config(), "");
        }
        "device-agent/config-set" => {
            let disabled: Vec<String> = payload
                .and_then(|p| p.get("disabledTools"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let expose_home = payload
                .and_then(|p| p.get("exposeHomeIndex"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            match set_device_agent_config(disabled, expose_home) {
                Ok(()) => reply(&window, id, true, json!({ "ok": true }), ""),
                Err(e) => reply(&window, id, false, Value::Null, &e),
            }
        }
        "app-autostart/get" => {
            let enabled = window
                .app_handle()
                .autolaunch()
                .is_enabled()
                .unwrap_or(false);
            reply(&window, id, true, json!({ "enabled": enabled }), "");
        }
        "app-autostart/set" => {
            let on = payload
                .and_then(|p| p.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mgr = window.app_handle().autolaunch();
            let r = if on { mgr.enable() } else { mgr.disable() };
            match r {
                Ok(()) => reply(&window, id, true, json!({ "enabled": on }), ""),
                Err(e) => reply(&window, id, false, Value::Null, &e.to_string()),
            }
        }
        _ => {
            // Unsupported (download/share/haptic/location/health/...). Reject anything that
            // expected a reply so the SPA degrades gracefully instead of hanging.
            reply(&window, id, false, Value::Null, "unsupported");
        }
    }
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

pub fn run(context: tauri::Context<tauri::Wry>) {
    // WebKitGTK on many Linux setups (Wayland, some GPU drivers, VMs) renders a blank white
    // page unless the DMABUF renderer is disabled. Set it before the webview starts; respect
    // an explicit override if the user already set it.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: single-threaded startup, before any webview/thread spawns.
        unsafe {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    tauri::Builder::default()
        // MUST be first: a 2nd launch hands its argv to the running instance, which just
        // focuses the existing window instead of opening a duplicate.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_server,
            set_server,
            forget_server,
            native_bridge
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            match read_server(&handle) {
                // Configured: open the SPA itself, with the native bridge injected.
                Some(server) => {
                    let url = tauri::Url::parse(&server).expect("stored server URL is invalid");
                    let app_base = base_domain(url.host_str().unwrap_or(""));
                    let nav_handle = handle.clone();
                    let window =
                        WebviewWindowBuilder::new(&handle, "main", WebviewUrl::External(url))
                            .title("Personal Agent")
                            .inner_size(1280.0, 860.0)
                            .min_inner_size(720.0, 560.0)
                            .center()
                            .initialization_script(BRIDGE_INIT_JS)
                            .on_navigation(move |nav_url| match nav_url.scheme() {
                                "http" | "https" => {
                                    if base_domain(nav_url.host_str().unwrap_or("")) == app_base {
                                        true // app + same-domain Keycloak stay in-window
                                    } else {
                                        // genuinely external -> system browser, don't navigate
                                        let _ = nav_handle
                                            .opener()
                                            .open_url(nav_url.as_str(), None::<&str>);
                                        false
                                    }
                                }
                                _ => true, // about:/blob:/data: allowed
                            })
                            .build()?;
                    let _ = window.restore_state(
                        StateFlags::POSITION | StateFlags::SIZE | StateFlags::MAXIMIZED,
                    );
                }
                // First run: the local server-setup screen (bundled ui/index.html).
                None => {
                    WebviewWindowBuilder::new(
                        &handle,
                        "main",
                        WebviewUrl::App("index.html".into()),
                    )
                    .title("Personal Agent")
                    .inner_size(560.0, 520.0)
                    .center()
                    .build()?;
                }
            }

            // System tray: the companion lives here when the window is closed-to-tray.
            let t = locale_map();
            let show = MenuItem::with_id(
                &handle,
                "show",
                tr(&t, "tray_open", "Open Personal Agent"),
                true,
                None::<&str>,
            )?;
            let change = MenuItem::with_id(
                &handle,
                "change",
                tr(&t, "tray_change", "Change server"),
                true,
                None::<&str>,
            )?;
            let hide = MenuItem::with_id(
                &handle,
                "hide",
                tr(&t, "tray_hide", "Hide to tray"),
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(
                &handle,
                "quit",
                tr(&t, "tray_quit", "Quit"),
                true,
                None::<&str>,
            )?;
            let menu = Menu::with_items(&handle, &[&show, &change, &hide, &quit])?;
            let _tray = TrayIconBuilder::with_id("main")
                .icon(handle.default_window_icon().unwrap().clone())
                .tooltip("Personal Agent")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "change" => forget_and_restart(app),
                    "hide" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(&handle)?;

            Ok(())
        })
        // Close = hide to tray (keep the SPA running in the background); quit via the tray.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(context)
        .expect("error while running the Personal Agent desktop app");
}
