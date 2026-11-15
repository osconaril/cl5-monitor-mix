//! Desktop shell for CL5 Monitor Mix.
//!
//! A thin Tauri wrapper: it starts the existing `cl5-server` binary as a child
//! process and points the webview at it on a loopback port. Because the web
//! app, the REST API, and the console WebSocket are all served from the same
//! origin, no Tauri IPC bridge is required.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent};

/// Loopback port the spawned server binds to (must match `tauri.conf.json`).
const PORT: u16 = 8787;
/// How long to wait for the server to start accepting connections.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Database file name, stored under the app-data directory.
const DB_FILE_NAME: &str = "cl5.sqlite";

/// The server child, so the app can terminate it on exit.
static SERVER: Mutex<Option<Child>> = Mutex::new(None);

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;

            let server_bin = resolve_server_bin(&resource_dir)?;
            let web_dist = resolve_web_dist(&resource_dir)?;
            let db_dir = app.path().app_data_dir()?;
            fs::create_dir_all(&db_dir)?;
            let db_path = db_dir.join(DB_FILE_NAME);

            let child = std::process::Command::new(&server_bin)
                .env("CL5_LISTEN", format!("127.0.0.1:{PORT}"))
                .env("CL5_WEB_DIST", web_dist)
                .env("CL5_DATABASE_PATH", db_path)
                .env("CL5_CONSOLE_ADDRESS", "127.0.0.1:49280")
                .spawn()
                .map_err(|error| format!("failed to start cl5-server ({server_bin:?}): {error}"))?;

            *SERVER.lock().unwrap() = Some(child);

            wait_until_ready()
                .map_err(|error| format!("{error}; is cl5-server built and healthy?"))?;

            if let Some(window) = app.get_webview_window("main") {
                // The first load raced the spawn; reload now that the server is up.
                window.eval("window.location.reload()")?;
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to start the CL5 Monitor Mix desktop app")
        .run(|_app, event| match event {
            RunEvent::Exit => {
                if let Some(mut server) = SERVER.lock().unwrap().take() {
                    let _ = server.kill();
                    let _ = server.wait();
                }
            }
            _ => {}
        });
}

/// Locate the `cl5-server` executable.
///
/// Resolution order: `CL5_DESKTOP_SERVER_BIN`, next to the current executable,
/// or inside the Tauri resource directory (the bundled layout).
fn resolve_server_bin(resource_dir: &Path) -> Result<PathBuf, String> {
    let binary_name = if cfg!(target_os = "windows") { "cl5-server.exe" } else { "cl5-server" };

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(value) = std::env::var("CL5_DESKTOP_SERVER_BIN") {
        candidates.push(PathBuf::from(value));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join(binary_name));
        }
    }
    candidates.push(resource_dir.join(binary_name));

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not find `{binary_name}`; build it with `cargo build -p cl5-server` and keep it next to the app"
    ))
}

/// Locate the built web app (the `index.html` plus the assets Trunk emits).
///
/// Resolution order: `CL5_DESKTOP_WEB_DIST`, the `resources/web` copy bundled
/// by Tauri, or the workspace `crates/web/dist` (the development layout).
fn resolve_web_dist(resource_dir: &Path) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(value) = std::env::var("CL5_DESKTOP_WEB_DIST") {
        candidates.push(PathBuf::from(value));
    }
    candidates.push(resource_dir.join("web"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // `target/debug/cl5-desktop.exe` -> `crates/web/dist`
            candidates.push(parent.join("../..").join("crates/web/dist"));
        }
    }

    for candidate in candidates {
        if candidate.join("index.html").is_file() {
            return Ok(candidate);
        }
    }

    Err("could not find a built web app; run `trunk build --dist dist` in `crates/web` first".to_string())
}

/// Block until something accepts TCP connections on the server port.
fn wait_until_ready() -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for the server on 127.0.0.1:{PORT}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
