# CL5 Monitor Mix

Personal monitor mixing for a Yamaha CL5 from any phone on the stage network.

The console allows only a limited number of connected mixing devices. This app connects to the console **once**, as a single client, and shares control with any number of phones and tablets through a small web server on the local network.

## Parts

- `crates/server` — the LAN server (Rust, native). Holds the one console connection, serves the web app, keeps users and their saved settings in SQLite, and pushes console changes to every open screen.
- `crates/web` — the web app (Rust compiled to WebAssembly). What performers and the monitor engineer use in a browser.

## Who uses it

- **Performers** sign in with a name and a 4-digit passcode and see only the mixes the engineer assigned them: send level per channel, pan on stereo-linked mixes, mute, a personal solo, their own channel order and channel groups.
- **The monitor engineer** uses the admin portal: add and delete users, reset passcodes, and assign mixes to users.

## Build

```
cargo test --workspace
cargo check --workspace --all-targets
node scripts/browser-smoke.mjs
```

## Web application

The phone UI is a Yew WebAssembly application. **Trunk builds it, not Cargo** —
`cargo build` and `cargo run` never recompile the wasm that the server serves.
After changing anything under `crates/web`, rebuild it with Trunk before
(re)starting the app, and hard-refresh the page so the browser drops its cached
`.wasm`/`.js` (on Windows: `Ctrl+Shift+R`).

### Run in a browser (LAN server)

```bash
cargo install trunk                          # one-time
cd crates/web && trunk build --dist dist
cd ../.. && cargo run -p cl5-server          # prints the panel URL (default 0.0.0.0:8080)
```

Open that URL on any device on the local network.

### Run as a desktop app (Tauri)

The desktop shell is a thin window: it starts `cl5-server` and points the webview
at `127.0.0.1:8787`. So build the wasm and the native server binary, then launch
the shell:

```bash
cd crates/web && trunk build --dist dist
cd ../..
./scripts/prepare-desktop-sidecar.sh         # builds the server sidecar Tauri packages
cargo build -p cl5-desktop
cargo run -p cl5-desktop
```

### Rebuild everything from scratch

```bash
cargo clean
cd crates/web && trunk build --release --dist dist
cd ../.. && cargo build -p cl5-server -p cl5-desktop
```

`prepare-desktop-sidecar.sh` builds `cl5-server` for the current target and
places it where Tauri's `externalBin` configuration expects it. Tauri copies
that sidecar alongside the desktop executable in a packaged application, so the
app can start its server without a separately installed binary.

For the full phone-size smoke check, run `node scripts/browser-smoke.mjs`; it
builds the Yew app into an isolated temporary directory before starting the
server.
