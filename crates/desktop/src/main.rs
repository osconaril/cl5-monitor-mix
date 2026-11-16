//! Desktop shell for CL5 Monitor Mix.
//!
//! A Tauri wrapper that lets an admin choose a real CL5 or the bundled fake
//! console before it starts the local server and opens the web UI.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, RunEvent};

/// Loopback port the spawned server binds to (must match `tauri.conf.json`).
const PORT: u16 = 8787;
/// How long to wait for the server to start accepting connections.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Database file name, stored under the app-data directory.
const DB_FILE_NAME: &str = "cl5.sqlite";

/// The server child, so the app can terminate it on exit.
static CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// Children started during one launch attempt.  They are only made visible to
/// the running app after every setup step succeeds; otherwise `Drop` tears
/// them down so a failed attempt can be retried.
#[derive(Default)]
struct PendingChildren(Vec<Child>);

impl PendingChildren {
    fn commit(mut self) {
        CHILDREN.lock().unwrap().append(&mut self.0);
    }
}

impl Drop for PendingChildren {
    fn drop(&mut self) {
        for mut child in self.0.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![launch_console_mode])
        .build(tauri::generate_context!())
        .expect("failed to start the CL5 Monitor Mix desktop app")
        .run(|_app, event| {
            if let RunEvent::Exit = event {
                for mut child in CHILDREN.lock().unwrap().drain(..) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        });
}

/// Starts the requested console mode, then starts the local web server.
#[tauri::command]
fn launch_console_mode(
    app: AppHandle,
    mode: String,
    address: Option<String>,
) -> Result<(), String> {
    if !CHILDREN.lock().unwrap().is_empty() {
        return Err("CL5 Monitor Mix is already running".into());
    }

    let mut children = PendingChildren::default();
    launch_console_mode_inner(app, mode, address, &mut children.0)?;
    children.commit();
    Ok(())
}

fn launch_console_mode_inner(
    app: AppHandle,
    mode: String,
    address: Option<String>,
    children: &mut Vec<Child>,
) -> Result<(), String> {
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?;
    let console_address = match mode.as_str() {
        "real" => address
            .filter(|value| !value.trim().is_empty())
            .ok_or("Enter the real CL5 address before connecting.")?,
        "fake" => {
            let fake_bin = resolve_fake_console_bin(&resource_dir)?;
            let fake = std::process::Command::new(&fake_bin)
                .env("FAKE_CONSOLE_ADDR", "127.0.0.1:49280")
                .spawn()
                .map_err(|error| format!("failed to start fake-console ({fake_bin:?}): {error}"))?;
            children.push(fake);
            "127.0.0.1:49280".into()
        }
        _ => return Err("Choose either the real CL5 or the fake console.".into()),
    };

    let server_bin = resolve_server_bin(&resource_dir)?;
    let web_dist = resolve_web_dist(&resource_dir)?;
    let db_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&db_dir).map_err(|error| error.to_string())?;
    let db_path = db_dir.join(DB_FILE_NAME);
    let server = std::process::Command::new(&server_bin)
        .env("CL5_LISTEN", format!("127.0.0.1:{PORT}"))
        .env("CL5_WEB_DIST", web_dist)
        .env("CL5_DATABASE_PATH", db_path)
        .env("CL5_CONSOLE_ADDRESS", console_address)
        .spawn()
        .map_err(|error| format!("failed to start cl5-server ({server_bin:?}): {error}"))?;
    children.push(server);

    wait_until_ready().map_err(|error| format!("{error}; is cl5-server built and healthy?"))?;
    app.get_webview_window("main")
        .ok_or("the main window is unavailable")?
        .eval("window.location.replace('http://127.0.0.1:8787/')")
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Locate the `cl5-server` executable.
///
/// Resolution order: `CL5_DESKTOP_SERVER_BIN`, next to the current executable,
/// or inside the Tauri resource directory (the bundled layout).
fn resolve_server_bin(resource_dir: &Path) -> Result<PathBuf, String> {
    resolve_sidecar_bin(resource_dir, "CL5_DESKTOP_SERVER_BIN", "cl5-server")
}

/// Locate the bundled fake console executable.
fn resolve_fake_console_bin(resource_dir: &Path) -> Result<PathBuf, String> {
    resolve_sidecar_bin(resource_dir, "CL5_DESKTOP_FAKE_CONSOLE_BIN", "fake-console")
}

fn resolve_sidecar_bin(
    resource_dir: &Path,
    environment_name: &str,
    binary_stem: &str,
) -> Result<PathBuf, String> {
    let binary_name = if cfg!(target_os = "windows") {
        format!("{binary_stem}.exe")
    } else {
        binary_stem.to_string()
    };

    let candidates = server_bin_candidates(
        resource_dir,
        std::env::var_os(environment_name),
        std::env::current_exe().ok(),
        &binary_name,
    );

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not find `{binary_name}`; prepare the desktop sidecars and keep it next to the app"
    ))
}

/// Build the lookup order for the server sidecar. Tauri copies `externalBin`
/// next to the desktop executable for both development and packaged builds.
fn server_bin_candidates(
    resource_dir: &Path,
    configured_path: Option<std::ffi::OsString>,
    executable_path: Option<PathBuf>,
    binary_name: &str,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = configured_path {
        candidates.push(PathBuf::from(value));
    }
    if let Some(executable_path) = executable_path {
        if let Some(parent) = executable_path.parent() {
            candidates.push(parent.join(binary_name));
        }
    }
    candidates.push(resource_dir.join(binary_name));
    candidates
}

/// Locate the built web app (the `index.html` plus the WebAssembly Trunk emits).
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
        if contains_built_web_app(&candidate) {
            return Ok(candidate);
        }
    }

    Err(
        "could not find a built web app; run `trunk build --dist dist` in `crates/web` first"
            .to_string(),
    )
}

/// A launch page alone is not a web build. The packaged tree must include the
/// generated WebAssembly asset which the admin and performer application loads.
fn contains_built_web_app(path: &Path) -> bool {
    path.join("index.html").is_file()
        && fs::read_dir(path)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "wasm")
            })
}

/// Block until something accepts TCP connections on the server port.
fn wait_until_ready() -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if TcpStream::connect(("127.0.0.1", PORT)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for the server on 127.0.0.1:{PORT}"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_sidecar_is_found_next_to_the_desktop_binary() {
        let root =
            std::env::temp_dir().join(format!("cl5-desktop-sidecar-test-{}", std::process::id()));
        let app_dir = root.join("CL5 Monitor Mix.app/Contents/MacOS");
        let resources = root.join("CL5 Monitor Mix.app/Contents/Resources");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&resources).unwrap();

        let server = app_dir.join("cl5-server");
        fs::write(&server, b"sidecar").unwrap();
        let candidates = server_bin_candidates(
            &resources,
            None,
            Some(app_dir.join("cl5-desktop")),
            "cl5-server",
        );

        assert_eq!(
            candidates.into_iter().find(|path| path.is_file()),
            Some(server)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packaged_web_tree_requires_generated_wasm() {
        let root =
            std::env::temp_dir().join(format!("cl5-desktop-web-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("index.html"), "<main>Starting</main>").unwrap();
        assert!(!contains_built_web_app(&root));

        fs::write(root.join("cl5_web_bg.wasm"), b"wasm").unwrap();
        assert!(contains_built_web_app(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_launch_cleans_up_children_before_a_retry() {
        assert!(CHILDREN.lock().unwrap().is_empty());
        let mut pending = PendingChildren::default();
        pending.0.push(
            std::process::Command::new("sh")
                .args(["-c", "sleep 30"])
                .spawn()
                .unwrap(),
        );

        // Dropping this failed attempt must not poison the process-wide
        // running state that the next launcher invocation checks.
        drop(pending);
        assert!(CHILDREN.lock().unwrap().is_empty());
    }
}
