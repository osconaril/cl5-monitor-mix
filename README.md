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
```
