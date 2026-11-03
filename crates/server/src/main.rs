//! Offline LAN sign-in server and the shared CL5 console bridge.
mod console;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Form, Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use rand::{distr::Alphanumeric, Rng};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tower_http::services::ServeDir;

type Db = Arc<Mutex<Connection>>;
#[derive(Clone)]
struct App {
    db: Db,
    // Login attempts are deliberately process-local: only sessions need to
    // survive a routine server restart.
    attempts: Arc<Mutex<HashMap<String, Attempt>>>,
    bridge: console::ConsoleBridge,
    live_connections: LiveConnections,
}

/// A phone counts as connected only while its signed-in browser has an open
/// console subscription. Sessions intentionally survive server restarts, but
/// a browser which has gone away must not remain visible as a live phone.
#[derive(Clone, Default)]
struct LiveConnections {
    entries: Arc<Mutex<HashMap<String, Vec<LiveConnection>>>>,
    active: Arc<AtomicUsize>,
    next_id: Arc<AtomicU64>,
}

struct LiveConnection {
    id: u64,
    user_name: String,
    shutdown: tokio::sync::broadcast::Sender<()>,
}

impl LiveConnections {
    fn connect(
        &self,
        token: String,
        user_name: String,
    ) -> (u64, tokio::sync::broadcast::Receiver<()>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (shutdown, receiver) = tokio::sync::broadcast::channel(1);
        self.entries
            .lock()
            .unwrap()
            .entry(token)
            .or_default()
            .push(LiveConnection {
                id,
                user_name,
                shutdown,
            });
        self.active.fetch_add(1, Ordering::Relaxed);
        (id, receiver)
    }

    fn disconnect(&self, token: &str, id: u64) {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            let Some(connections) = entries.get_mut(token) else {
                return;
            };
            let before = connections.len();
            connections.retain(|connection| connection.id != id);
            let removed = before != connections.len();
            if connections.is_empty() {
                entries.remove(token);
            }
            removed
        };
        if removed {
            self.active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    fn disconnect_user(&self, user_name: &str) {
        let shutdown = {
            let mut entries = self.entries.lock().unwrap();
            let mut shutdown = Vec::new();
            for connections in entries.values_mut() {
                connections.retain(|connection| {
                    if connection.user_name == user_name {
                        shutdown.push(connection.shutdown.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            entries.retain(|_, connections| !connections.is_empty());
            shutdown
        };
        if !shutdown.is_empty() {
            self.active.fetch_sub(shutdown.len(), Ordering::Relaxed);
            for connection in shutdown {
                let _ = connection.send(());
            }
        }
    }

    fn disconnect_token(&self, token: &str) {
        let shutdown = self
            .entries
            .lock()
            .unwrap()
            .remove(token)
            .unwrap_or_default();
        if !shutdown.is_empty() {
            self.active.fetch_sub(shutdown.len(), Ordering::Relaxed);
            for connection in shutdown {
                let _ = connection.shutdown.send(());
            }
        }
    }

    fn count(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}
#[derive(Clone)]
struct User {
    name: String,
    role: String,
    updated_at: String,
}
#[derive(Serialize)]
struct Bootstrap {
    has_admin: bool,
    user: Option<BootstrapUser>,
    users: Vec<String>,
}
#[derive(Serialize)]
struct AdminOverview {
    users: Vec<AdminUser>,
    history: Vec<HistoryRow>,
    console: ConsoleStatus,
}
#[derive(Serialize)]
struct AdminUser {
    name: String,
    role: String,
    passcode: String,
    mixes: Vec<String>,
    last_sign_in: Option<String>,
}
#[derive(Serialize)]
struct HistoryRow {
    who: String,
    what: String,
    when: String,
}
#[derive(Serialize)]
struct ConsoleStatus {
    connected: bool,
    address: String,
    scene_number: Option<u16>,
    scene_name: Option<String>,
    mix_labels: Vec<String>,
    phones_connected: usize,
}
#[derive(Serialize)]
struct PerformerMixes {
    mixes: Vec<String>,
}
#[derive(Serialize)]
struct BootstrapUser {
    name: String,
    role: String,
}
struct Attempt {
    failures: u8,
    locked_until: Option<Instant>,
}
#[derive(Deserialize)]
struct Credentials {
    name: String,
    passcode: String,
}
#[derive(Deserialize)]
struct NewUser {
    name: String,
    role: String,
    passcode: String,
}
#[derive(Deserialize)]
struct Passcode {
    passcode: String,
}
#[derive(Deserialize)]
struct Mixes {
    #[serde(default)]
    mixes: Vec<String>,
}
#[derive(Deserialize)]
struct ConsoleChange {
    path: String,
    input: u8,
    mix: u8,
    value: String,
}
#[derive(Serialize)]
struct ConsoleSnapshot {
    kind: &'static str,
    connected: bool,
    state: console::ConsoleState,
}

#[tokio::main]
async fn main() {
    let path = env::var("CL5_DATABASE_PATH").unwrap_or_else(|_| "cl5-monitor-mix.sqlite".into());
    let address: SocketAddr = env::var("CL5_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .expect("CL5_LISTEN must be host:port");
    let console_address =
        env::var("CL5_CONSOLE_ADDRESS").unwrap_or_else(|_| "127.0.0.1:49280".into());
    let app = App::open(&path)
        .expect("open SQLite database")
        .with_console(console_address);
    if !app.has_admin().expect("read users") {
        let lan_address =
            local_ip_address::local_ip().expect("find a reachable LAN address for first-run setup");
        println!("{}", setup_instruction(lan_address, address.port()));
    }
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("bind CL5_LISTEN");
    println!("CL5 Monitor Mix listening on http://{address}");
    axum::serve(listener, routes(app))
        .await
        .expect("serve HTTP");
}
fn setup_instruction(lan_address: std::net::IpAddr, port: u16) -> String {
    format!("First-run setup: open http://{lan_address}:{port}/setup from the stage network.")
}
fn routes(app: App) -> Router {
    Router::new()
        .route("/", get(web_app))
        .route("/setup", get(web_app).post(setup))
        .route("/signin", post(signin))
        .route("/signout", post(signout))
        .route("/api/admin/ping", get(admin_ping))
        .route("/api/admin/overview", get(admin_overview))
        .route("/api/admin/users", post(create_user))
        .route("/api/admin/users/{name}/passcode", post(reset_passcode))
        .route("/api/admin/users/{name}/mixes", post(assign_mixes))
        .route("/api/performer/mixes", get(performer_mixes))
        .route(
            "/api/admin/users/{name}",
            post(delete_user).delete(delete_user),
        )
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/console", get(console_socket).post(console_change))
        .with_state(app)
        .fallback_service(ServeDir::new(web_dist()))
}
impl App {
    fn open(path: &str) -> rusqlite::Result<Self> {
        let mut c = Connection::open(path)?;
        migrate(&mut c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            attempts: Default::default(),
            bridge: console::ConsoleBridge::offline(),
            live_connections: Default::default(),
        })
    }
    #[cfg(test)]
    fn memory() -> rusqlite::Result<Self> {
        let mut c = Connection::open_in_memory()?;
        migrate(&mut c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            attempts: Default::default(),
            bridge: console::ConsoleBridge::offline(),
            live_connections: Default::default(),
        })
    }
    fn with_console(mut self, address: String) -> Self {
        self.bridge = console::ConsoleBridge::start(address);
        self
    }
    #[cfg(test)]
    fn with_bridge(mut self, bridge: console::ConsoleBridge) -> Self {
        self.bridge = bridge;
        self
    }
    fn has_admin(&self) -> rusqlite::Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE role='admin')",
            [],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }
    #[cfg(test)]
    fn add_user(&self, n: &str, p: &str, role: &str) -> Result<(), String> {
        valid(n, p)?;
        let now = Utc::now().to_rfc3339();
        self.db.lock().unwrap().execute("INSERT INTO users(name,passcode,role,created_at,updated_at) VALUES(?1,?2,?3,?4,?4)",params![n.trim(),p,role,now]).map(|_|()).map_err(|_|"That name is already in use.".into())
    }
    fn add_first_admin(&self, n: &str, p: &str) -> Result<(), String> {
        valid(n, p)?;
        let mut connection = self.db.lock().unwrap();
        let transaction = connection
            .transaction()
            .map_err(|_| "Could not create the first admin. Try again.".to_string())?;
        let has_admin: i64 = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE role='admin')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| "Could not create the first admin. Try again.".to_string())?;
        if has_admin != 0 {
            return Err("An administrator already exists. Sign in instead.".into());
        }
        let now = Utc::now().to_rfc3339();
        transaction
            .execute(
                "INSERT INTO users(name,passcode,role,created_at,updated_at) VALUES(?1,?2,'admin',?3,?3)",
                params![n.trim(), p, now],
            )
            .map_err(|_| "That name is already in use. Try another name.".to_string())?;
        transaction
            .commit()
            .map_err(|_| "Could not create the first admin. Try again.".to_string())
    }
    fn user(&self, n: &str, p: &str) -> rusqlite::Result<Option<User>> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT name,role,updated_at FROM users WHERE name=?1 AND passcode=?2",
                params![n.trim(), p],
                |r| {
                    Ok(User {
                        name: r.get(0)?,
                        role: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                },
            )
            .optional()
    }
    fn user_names(&self) -> rusqlite::Result<Vec<String>> {
        let connection = self.db.lock().unwrap();
        let mut statement =
            connection.prepare("SELECT name FROM users ORDER BY name COLLATE NOCASE")?;
        let names = statement.query_map([], |row| row.get(0))?.collect();
        names
    }
    async fn admin_overview(&self) -> rusqlite::Result<AdminOverview> {
        let (users, history, phones_connected) = {
            let c = self.db.lock().unwrap();
            let mut statement = c.prepare(
                "SELECT name, role, passcode, last_sign_in FROM users ORDER BY name COLLATE NOCASE",
            )?;
            let users = statement
                .query_map([], |r| {
                    let name: String = r.get(0)?;
                    let mixes = c
                        .prepare(
                            "SELECT mix_name FROM user_mixes WHERE user_name=?1 ORDER BY mix_name",
                        )?
                        .query_map([&name], |m| m.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?;
                    Ok(AdminUser {
                        name,
                        role: r.get(1)?,
                        passcode: r.get(2)?,
                        mixes,
                        last_sign_in: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut history_statement = c.prepare(
                "SELECT who, what, happened_at FROM admin_history ORDER BY id DESC LIMIT 20",
            )?;
            let history = history_statement
                .query_map([], |r| {
                    Ok(HistoryRow {
                        who: r.get(0)?,
                        what: r.get(1)?,
                        when: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let phones_connected = self.live_connections.count();
            (users, history, phones_connected)
        };
        let state = self.bridge.state().await;
        let mix_labels = (0..24)
            .map(|mix| {
                let key = format!("MIXER:Current/Mix/Label/Name {mix} 0");
                state
                    .values
                    .get(&key)
                    .filter(|label| !label.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("Mix {}", mix + 1))
            })
            .collect();
        Ok(AdminOverview {
            users,
            history,
            console: ConsoleStatus {
                connected: self.bridge.is_connected(),
                address: env::var("CL5_CONSOLE_ADDRESS")
                    .unwrap_or_else(|_| "Not configured".into()),
                scene_number: state.scene_number,
                scene_name: state.scene_name,
                mix_labels,
                phones_connected,
            },
        })
    }
    fn audit(&self, who: &str, what: &str) -> rusqlite::Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO admin_history(who,what,happened_at) VALUES(?1,?2,?3)",
            params![who, what, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
    fn user_mixes(&self, name: &str) -> rusqlite::Result<Vec<String>> {
        let c = self.db.lock().unwrap();
        let mixes = c
            .prepare("SELECT mix_name FROM user_mixes WHERE user_name=?1 ORDER BY mix_name")?
            .query_map([name], |row| row.get(0))?
            .collect();
        mixes
    }
    fn create_user(&self, actor: &str, f: &NewUser) -> Result<String, String> {
        let passcode = if f.passcode == "Generate" {
            generate_passcode()
        } else {
            f.passcode.clone()
        };
        valid(&f.name, &passcode)?;
        if !matches!(f.role.as_str(), "admin" | "performer") {
            return Err("Choose admin or performer.".into());
        }
        let now = Utc::now().to_rfc3339();
        self.db.lock().unwrap().execute("INSERT INTO users(name,passcode,role,created_at,updated_at) VALUES(?1,?2,?3,?4,?4)", params![f.name.trim(), passcode, f.role, now]).map_err(|_| "That name is already in use.".to_string())?;
        self.audit(actor, &format!("Added {} ({})", f.name.trim(), f.role))
            .map_err(|_| "Could not save history.".to_string())?;
        Ok(passcode)
    }
    fn reset_passcode(&self, actor: &str, name: &str, requested: &str) -> Result<String, String> {
        let passcode = if requested == "Generate" {
            generate_passcode()
        } else {
            requested.into()
        };
        valid(name, &passcode)?;
        let now = Utc::now().to_rfc3339();
        let changed = self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE users SET passcode=?1,updated_at=?2 WHERE name=?3",
                params![passcode, now, name],
            )
            .map_err(|_| "Could not reset passcode.".to_string())?;
        if changed == 0 {
            return Err("That user no longer exists.".into());
        }
        self.live_connections.disconnect_user(name);
        self.audit(actor, &format!("Reset passcode for {name}"))
            .map_err(|_| "Could not save history.".to_string())?;
        Ok(passcode)
    }
    fn delete_user(&self, actor: &str, name: &str) -> Result<(), String> {
        if actor == name {
            return Err("You cannot delete yourself.".into());
        }
        let c = self.db.lock().unwrap();
        let role: Option<String> = c
            .query_row("SELECT role FROM users WHERE name=?1", [name], |r| r.get(0))
            .optional()
            .map_err(|_| "Could not find that user.".to_string())?;
        if role.as_deref() == Some("admin")
            && c.query_row("SELECT COUNT(*) FROM users WHERE role='admin'", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0)
                <= 1
        {
            return Err("The last admin cannot be deleted.".into());
        }
        c.execute("DELETE FROM user_mixes WHERE user_name=?1", [name])
            .map_err(|_| "Could not delete saved settings.".to_string())?;
        c.execute("DELETE FROM sessions WHERE user_name=?1", [name])
            .map_err(|_| "Could not sign out that user.".to_string())?;
        if c.execute("DELETE FROM users WHERE name=?1", [name])
            .map_err(|_| "Could not delete that user.".to_string())?
            == 0
        {
            return Err("That user no longer exists.".into());
        }
        drop(c);
        self.live_connections.disconnect_user(name);
        self.audit(actor, &format!("Deleted {name} and their saved settings"))
            .map_err(|_| "Could not save history.".to_string())
    }
    fn assign_mixes(&self, actor: &str, name: &str, mixes: &[String]) -> Result<(), String> {
        let c = self.db.lock().unwrap();
        if c.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE name=?1)",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            == 0
        {
            return Err("That user no longer exists.".into());
        }
        let tx = c
            .unchecked_transaction()
            .map_err(|_| "Could not save mix assignments.".to_string())?;
        tx.execute("DELETE FROM user_mixes WHERE user_name=?1", [name])
            .map_err(|_| "Could not save mix assignments.".to_string())?;
        for mix in canonical_mix_numbers(mixes)? {
            tx.execute(
                "INSERT INTO user_mixes(user_name,mix_name) VALUES(?1,?2)",
                params![name, mix],
            )
            .map_err(|_| "Could not save mix assignments.".to_string())?;
        }
        tx.commit()
            .map_err(|_| "Could not save mix assignments.".to_string())?;
        drop(c);
        self.audit(actor, &format!("Changed mix assignments for {name}"))
            .map_err(|_| "Could not save history.".to_string())
    }
}
fn canonical_mix_numbers(mixes: &[String]) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for value in mixes {
        let number = value
            .trim()
            .parse::<u8>()
            .map_err(|_| "Choose mix numbers from 1 through 24.".to_string())?;
        if !(1..=24).contains(&number) {
            return Err("Choose mix numbers from 1 through 24.".into());
        }
        let mix = format!("Mix {number}");
        if !result.contains(&mix) {
            result.push(mix);
        }
    }
    Ok(result)
}
fn migrate(c: &mut Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version TEXT PRIMARY KEY, applied_at TEXT NOT NULL);",
    )?;
    apply_migration(
        c,
        "0001_users",
        include_str!("../migrations/0001_users.sql"),
    )?;
    apply_migration(
        c,
        "0002_admin_portal",
        include_str!("../migrations/0002_admin_portal.sql"),
    )?;
    Ok(())
}
fn apply_migration(c: &mut Connection, version: &str, sql: &str) -> rusqlite::Result<()> {
    let transaction = c.transaction()?;
    let applied = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version=?1)",
        [version],
        |row| row.get::<_, i64>(0),
    )?;
    if applied == 0 {
        transaction.execute_batch(sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES(?1, ?2)",
            params![version, Utc::now().to_rfc3339()],
        )?;
    }
    transaction.commit()?;
    Ok(())
}
fn valid(n: &str, p: &str) -> Result<(), String> {
    if n.trim().is_empty() {
        Err("Enter your name.".into())
    } else if p.len() != 4 || !p.bytes().all(|c| c.is_ascii_digit()) {
        Err("Passcode must be exactly 4 digits.".into())
    } else {
        Ok(())
    }
}
fn web_dist() -> PathBuf {
    env::var_os("CL5_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/web/dist"))
}
async fn web_app() -> Response {
    let index = std::fs::read_to_string(web_dist().join("index.html"))
        .unwrap_or_else(|_| include_str!("../../web/index.html").to_owned());
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], index).into_response()
}
async fn bootstrap(State(a): State<App>, headers: HeaderMap) -> Json<Bootstrap> {
    let user = session(&a, &headers).map(|user| BootstrapUser {
        name: user.name,
        role: user.role,
    });
    Json(Bootstrap {
        has_admin: a.has_admin().unwrap_or(true),
        user,
        users: a.user_names().unwrap_or_default(),
    })
}
async fn console_socket(
    ws: WebSocketUpgrade,
    State(a): State<App>,
    headers: HeaderMap,
) -> Response {
    let Some(token) = cookie(&headers).map(str::to_owned) else {
        return (StatusCode::UNAUTHORIZED, "Sign in first.").into_response();
    };
    let Some(user) = session(&a, &headers) else {
        return (StatusCode::UNAUTHORIZED, "Sign in first.").into_response();
    };
    ws.on_upgrade(move |socket| {
        stream_console(socket, a.bridge, a.live_connections, token, user.name)
    })
    .into_response()
}

async fn stream_console(
    mut socket: WebSocket,
    bridge: console::ConsoleBridge,
    live_connections: LiveConnections,
    token: String,
    user_name: String,
) {
    let (connection_id, mut shutdown) = live_connections.connect(token.clone(), user_name);
    if send_console_snapshot(&mut socket, &bridge).await.is_err() {
        live_connections.disconnect(&token, connection_id);
        return;
    }
    let mut events = bridge.subscribe();
    loop {
        let event = tokio::select! {
            _ = shutdown.recv() => break,
            event = events.recv() => event,
        };
        match event {
            Ok(event) => {
                if send_socket(&mut socket, &event).await.is_err() {
                    break;
                }
            }
            // A scene rehydration deliberately emits more individual values
            // than a slow browser's event buffer can hold. The bridge state is
            // authoritative, so replace the missed delta with one fresh
            // snapshot and continue listening for later changes.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if send_console_snapshot(&mut socket, &bridge).await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    live_connections.disconnect(&token, connection_id);
}

async fn send_console_snapshot(
    socket: &mut WebSocket,
    bridge: &console::ConsoleBridge,
) -> Result<(), ()> {
    send_socket(
        socket,
        &ConsoleSnapshot {
            kind: "snapshot",
            connected: bridge.is_connected(),
            state: bridge.state().await,
        },
    )
    .await
}

async fn send_socket<T: Serialize>(socket: &mut WebSocket, message: &T) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    socket
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

async fn console_change(
    State(a): State<App>,
    headers: HeaderMap,
    Json(change): Json<ConsoleChange>,
) -> Response {
    let Some(user) = session(&a, &headers) else {
        return (StatusCode::UNAUTHORIZED, "Sign in first.").into_response();
    };
    if !may_change_mix(&a, &user, change.mix) {
        return (StatusCode::FORBIDDEN, "That mix is not assigned to you.").into_response();
    }
    let change = console::PhoneChange {
        path: change.path,
        input: change.input,
        mix: change.mix,
        value: change.value,
    };
    match a.bridge.change(change).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}

/// Performers are limited to the mix numbers recorded by the admin portal;
/// admins retain emergency control of every mix.
fn may_change_mix(a: &App, user: &User, mix: u8) -> bool {
    user.role == "admin" || assigned_mixes(a, &user.name).contains(&mix)
}

fn assigned_mixes(a: &App, user_name: &str) -> HashSet<u8> {
    a.user_mixes(user_name)
        .unwrap_or_default()
        .into_iter()
        // The UI stores the one-based display number, while Yamaha's protocol
        // uses zero-based mix indices. Keep that conversion at this boundary.
        .filter_map(|mix| mix.strip_prefix("Mix ")?.parse::<u8>().ok()?.checked_sub(1))
        .collect()
}
async fn setup(State(a): State<App>, Form(f): Form<Credentials>) -> Response {
    match a.add_first_admin(&f.name, &f.passcode) {
        Ok(()) => login(&a, a.user(&f.name, &f.passcode).unwrap().unwrap()),
        Err(_) => Redirect::to("/setup?error=setup").into_response(),
    }
}
async fn signin(State(a): State<App>, Form(f): Form<Credentials>) -> Response {
    let mut attempts = a.attempts.lock().unwrap();
    let entry = attempts.entry(f.name.trim().into()).or_insert(Attempt {
        failures: 0,
        locked_until: None,
    });
    if let Some(until) = entry.locked_until {
        if until > Instant::now() {
            return locked_redirect(until).into_response();
        }
        entry.failures = 0;
        entry.locked_until = None
    }
    drop(attempts);
    match a.user(&f.name, &f.passcode).unwrap_or(None) {
        Some(u) => {
            a.attempts.lock().unwrap().remove(f.name.trim());
            login(&a, u)
        }
        None => {
            let mut m = a.attempts.lock().unwrap();
            let e = m.entry(f.name.trim().into()).or_insert(Attempt {
                failures: 0,
                locked_until: None,
            });
            e.failures += 1;
            if e.failures >= 5 {
                let until = Instant::now() + Duration::from_secs(300);
                e.locked_until = Some(until);
                return locked_redirect(until).into_response();
            }
            Redirect::to("/?error=wrong").into_response()
        }
    }
}
fn locked_redirect(until: Instant) -> Redirect {
    let seconds = until
        .saturating_duration_since(Instant::now())
        .as_secs()
        .max(1);
    Redirect::to(&format!("/?error=locked&seconds={seconds}"))
}
async fn signout(State(a): State<App>, h: HeaderMap) -> Response {
    if let Some(t) = cookie(&h) {
        let _ =
            a.db.lock()
                .unwrap()
                .execute("DELETE FROM sessions WHERE token=?1", [t]);
        a.live_connections.disconnect_token(t);
    }
    Redirect::to("/").into_response()
}
async fn admin_ping(State(a): State<App>, h: HeaderMap) -> Response {
    match session(&a, &h) {
        Some(u) if u.role == "admin" => (StatusCode::OK, "Admin access confirmed.").into_response(),
        Some(_) => (StatusCode::FORBIDDEN, "Admins only. Sign in as an admin.").into_response(),
        None => (StatusCode::UNAUTHORIZED, "Sign in first.").into_response(),
    }
}
fn admin(a: &App, h: &HeaderMap) -> Result<User, StatusCode> {
    match session(a, h) {
        Some(u) if u.role == "admin" => Ok(u),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}
async fn admin_overview(State(a): State<App>, h: HeaderMap) -> Response {
    if let Err(status) = admin(&a, &h) {
        return (
            status,
            if status == StatusCode::FORBIDDEN {
                "Admins only."
            } else {
                "Sign in first."
            },
        )
            .into_response();
    }
    match a.admin_overview().await {
        Ok(overview) => Json(overview).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load admin data.",
        )
            .into_response(),
    }
}
async fn create_user(State(a): State<App>, h: HeaderMap, Form(f): Form<NewUser>) -> Response {
    let actor = match admin(&a, &h) {
        Ok(actor) => actor,
        Err(status) => return status.into_response(),
    };
    match a.create_user(&actor.name, &f) {
        Ok(_) => Redirect::to("/").into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}
async fn reset_passcode(
    State(a): State<App>,
    h: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<Passcode>,
) -> Response {
    let actor = match admin(&a, &h) {
        Ok(actor) => actor,
        Err(status) => return status.into_response(),
    };
    match a.reset_passcode(&actor.name, &name, &f.passcode) {
        Ok(_) => Redirect::to("/").into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}
async fn assign_mixes(
    State(a): State<App>,
    h: HeaderMap,
    Path(name): Path<String>,
    Form(f): Form<Mixes>,
) -> Response {
    let actor = match admin(&a, &h) {
        Ok(actor) => actor,
        Err(status) => return status.into_response(),
    };
    match a.assign_mixes(&actor.name, &name, &f.mixes) {
        Ok(()) => Redirect::to("/").into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}
async fn performer_mixes(State(a): State<App>, h: HeaderMap) -> Response {
    match session(&a, &h) {
        Some(user) => match a.user_mixes(&user.name) {
            Ok(mixes) => Json(PerformerMixes { mixes }).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load mix assignments.",
            )
                .into_response(),
        },
        None => (StatusCode::UNAUTHORIZED, "Sign in first.").into_response(),
    }
}
async fn delete_user(State(a): State<App>, h: HeaderMap, Path(name): Path<String>) -> Response {
    let actor = match admin(&a, &h) {
        Ok(actor) => actor,
        Err(status) => return status.into_response(),
    };
    match a.delete_user(&actor.name, &name) {
        Ok(()) => Redirect::to("/").into_response(),
        Err(message) => (StatusCode::BAD_REQUEST, message).into_response(),
    }
}
fn login(a: &App, u: User) -> Response {
    let t: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    a.db.lock()
        .unwrap()
        .execute(
            "INSERT INTO sessions(token,user_name,user_updated_at,created_at) VALUES(?1,?2,?3,?4)",
            params![&t, &u.name, &u.updated_at, Utc::now().to_rfc3339()],
        )
        .expect("store session");
    let _ = a.db.lock().unwrap().execute(
        "UPDATE users SET last_sign_in=?1 WHERE name=?2",
        params![Utc::now().to_rfc3339(), &u.name],
    );
    (
        [
            (
                header::SET_COOKIE,
                format!("cl5_session={t}; HttpOnly; SameSite=Strict; Path=/"),
            ),
            (header::LOCATION, "/".to_string()),
        ],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}
fn generate_passcode() -> String {
    format!("{:04}", rand::rng().random_range(0..10_000))
}
fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|x| x.strip_prefix("cl5_session="))
}
fn session(a: &App, h: &HeaderMap) -> Option<User> {
    let token = cookie(h)?;
    a.db
        .lock()
        .unwrap()
        .query_row(
            "SELECT u.name,u.role,u.updated_at FROM sessions s JOIN users u ON u.name=s.user_name WHERE s.token=?1 AND s.user_updated_at=u.updated_at",
            [token],
            |r| Ok(User { name: r.get(0)?, role: r.get(1)?, updated_at: r.get(2)? }),
        )
        .optional()
        .ok()?
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use futures_util::StreamExt;
    use tokio::{net::TcpListener, time::timeout};
    use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
    use tower::ServiceExt;

    async fn response_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    async fn call(app: &App, request: Request<Body>) -> Response {
        routes(app.clone()).oneshot(request).await.unwrap()
    }

    async fn next_websocket_text<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> String
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        timeout(Duration::from_secs(15), async {
            loop {
                match socket.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        break text.to_string()
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => panic!("websocket read failed: {error}"),
                    None => panic!("websocket closed"),
                }
            }
        })
        .await
        .expect("websocket response arrives")
    }

    #[tokio::test]
    async fn websocket_resynchronizes_after_scene_recall_and_stays_live() {
        use fake_console::FakeConsole;

        let fake = Arc::new(FakeConsole::new());
        let console_address = fake.clone().listen("127.0.0.1:0").await.unwrap();
        let bridge = console::ConsoleBridge::start_for_lag_test(console_address.to_string());
        let app = App::memory().unwrap().with_bridge(bridge.clone());
        app.add_user("Admin", "1234", "admin").unwrap();
        timeout(Duration::from_secs(15), async {
            while !bridge.is_connected() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bridge connects to fake console");

        let login_response = login(&app, app.user("Admin", "1234").unwrap().unwrap());
        let cookie = login_response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, routes(app)).await });

        let mut request = format!("ws://{address}/api/console")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        let (mut socket, _) = connect_async(request).await.unwrap();
        assert!(next_websocket_text(&mut socket)
            .await
            .contains("\"kind\":\"snapshot\""));

        fake.recall_scene(2).await.unwrap();
        let resynchronized = timeout(Duration::from_secs(5), async {
            loop {
                if next_websocket_text(&mut socket)
                    .await
                    .contains("\"kind\":\"snapshot\"")
                {
                    break;
                }
            }
        })
        .await;
        assert!(
            resynchronized.is_ok(),
            "scene recall resynchronizes the WebSocket"
        );

        timeout(Duration::from_secs(15), async {
            while !bridge.is_connected() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("bridge rehydrates before accepting the later phone change");

        bridge
            .change(console::PhoneChange {
                path: console::LEVEL.into(),
                input: 0,
                mix: 0,
                value: "-2".into(),
            })
            .await
            .unwrap();
        let later_change = timeout(Duration::from_secs(5), async {
            loop {
                if next_websocket_text(&mut socket)
                    .await
                    .contains("\"value\":\"-2\"")
                {
                    break;
                }
            }
        })
        .await;
        server.abort();
        assert!(
            later_change.is_ok(),
            "the same WebSocket receives later console changes"
        );
    }

    #[test]
    fn database_migrates_and_accepts_admin() {
        let a = App::memory().unwrap();
        assert!(!a.has_admin().unwrap());
        a.add_user("Corey", "1234", "admin").unwrap();
        assert!(a.has_admin().unwrap());
        assert_eq!(a.user("Corey", "1234").unwrap().unwrap().role, "admin");
    }

    #[test]
    fn migration_ledger_skips_an_already_applied_migration() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (version TEXT PRIMARY KEY, applied_at TEXT NOT NULL);",
            )
            .unwrap();
        apply_migration(
            &mut connection,
            "test_once",
            "CREATE TABLE migration_runs (value TEXT NOT NULL);",
        )
        .unwrap();

        // A restarted process must consult the version ledger before attempting
        // the SQL again. This SQL would fail if it were replayed.
        apply_migration(&mut connection, "test_once", "THIS IS NOT VALID SQL").unwrap();
        let runs: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version='test_once'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(runs, 1);
    }
    #[test]
    fn passcodes_are_four_digits() {
        assert!(valid("a", "1234").is_ok());
        assert!(valid("a", "123").is_err());
        assert!(valid("a", "abcd").is_err());
    }

    #[test]
    fn performers_are_denied_unassigned_mixes_while_admins_can_operate_any_mix() {
        let app = App::memory().unwrap();
        app.add_user("Performer", "1234", "performer").unwrap();
        app.assign_mixes("Admin", "Performer", &["1".into(), "24".into()])
            .unwrap();
        let performer = User {
            name: "Performer".into(),
            role: "performer".into(),
            updated_at: "now".into(),
        };
        let admin = User {
            name: "Admin".into(),
            role: "admin".into(),
            updated_at: "now".into(),
        };
        assert!(
            may_change_mix(&app, &performer, 0),
            "Mix 1 is protocol index 0"
        );
        assert!(!may_change_mix(&app, &performer, 1));
        assert!(
            may_change_mix(&app, &performer, 23),
            "Mix 24 is protocol index 23"
        );
        assert!(may_change_mix(&app, &admin, 23));
    }

    #[tokio::test]
    async fn reserved_characters_in_user_names_reach_admin_actions() {
        let app = App::memory().unwrap();
        app.add_user("Admin", "1234", "admin").unwrap();
        app.add_user("A/B#?", "5678", "performer").unwrap();
        let login_response = login(&app, app.user("Admin", "1234").unwrap().unwrap());
        let cookie = login_response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();

        let response = call(
            &app,
            Request::post("/api/admin/users/A%2FB%23%3F/passcode")
                .header(header::COOKIE, cookie)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("passcode=4321"))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(app.user("A/B#?", "4321").unwrap().is_some());
    }

    #[tokio::test]
    async fn resetting_a_user_closes_their_open_console_socket() {
        let app = App::memory().unwrap();
        app.add_user("Admin", "1234", "admin").unwrap();
        app.add_user("Performer", "5678", "performer").unwrap();
        let login_response = login(&app, app.user("Performer", "5678").unwrap().unwrap());
        let cookie = login_response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_app = app.clone();
        let server = tokio::spawn(async move { axum::serve(listener, routes(server_app)).await });

        let mut request = format!("ws://{address}/api/console")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        let (mut socket, _) = connect_async(request).await.unwrap();
        assert!(next_websocket_text(&mut socket)
            .await
            .contains("\"kind\":\"snapshot\""));
        assert_eq!(app.live_connections.count(), 1);

        app.reset_passcode("Admin", "Performer", "9999").unwrap();
        let closed = timeout(Duration::from_secs(5), socket.next()).await;
        server.abort();
        assert!(matches!(
            closed,
            Ok(None)
                | Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))))
                // Axum drops a server-side socket without a close frame here;
                // tungstenite reports that peer closure as a protocol error.
                | Ok(Some(Err(_)))
        ));
        assert_eq!(app.live_connections.count(), 0);
    }

    #[test]
    fn first_run_instruction_is_a_complete_lan_url() {
        assert_eq!(
            setup_instruction("192.168.1.42".parse().unwrap(), 8080),
            "First-run setup: open http://192.168.1.42:8080/setup from the stage network."
        );
    }

    #[tokio::test]
    async fn first_admin_setup_only_works_once() {
        let app = App::memory().unwrap();
        let setup = call(&app, Request::get("/setup").body(Body::empty()).unwrap()).await;
        assert_eq!(setup.status(), StatusCode::OK);
        assert!(response_text(setup).await.contains("data-trunk"));

        let created = call(
            &app,
            Request::post("/setup")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=Corey&passcode=1234"))
                .unwrap(),
        )
        .await;
        assert_eq!(created.status(), StatusCode::SEE_OTHER);
        assert!(created.headers().contains_key(header::SET_COOKIE));

        let again = call(&app, Request::get("/setup").body(Body::empty()).unwrap()).await;
        assert_eq!(again.status(), StatusCode::OK);
    }

    #[test]
    fn first_admin_creation_is_atomic() {
        let app = App::memory().unwrap();
        assert!(app.add_first_admin("Corey", "1234").is_ok());
        assert!(app.add_first_admin("Second", "5678").is_err());
        let count: i64 = app
            .db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM users WHERE role='admin'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn sign_in_lockout_and_admin_authorization_are_http_checked() {
        let app = App::memory().unwrap();
        app.add_user("Admin", "1234", "admin").unwrap();
        app.add_user("Performer", "5678", "performer").unwrap();
        for attempt in 1..=5 {
            let response = call(
                &app,
                Request::post("/signin")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("name=Performer&passcode=0000"))
                    .unwrap(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            if attempt == 5 {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .unwrap()
                    .to_str()
                    .unwrap();
                assert!(location.starts_with("/?error=locked&seconds="));
                let seconds: u64 = location.rsplit('=').next().unwrap().parse().unwrap();
                assert!((299..=300).contains(&seconds));
            }
        }
        let locked = call(
            &app,
            Request::post("/signin")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=Performer&passcode=5678"))
                .unwrap(),
        )
        .await;
        assert_eq!(locked.status(), StatusCode::SEE_OTHER);
        let location = locked
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(location.starts_with("/?error=locked&seconds="));
        let seconds: u64 = location.rsplit('=').next().unwrap().parse().unwrap();
        assert!((1..=300).contains(&seconds));

        let admin = call(
            &app,
            Request::post("/signin")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=Admin&passcode=1234"))
                .unwrap(),
        )
        .await;
        let cookie = admin
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let permitted = call(
            &app,
            Request::get("/api/admin/ping")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(permitted.status(), StatusCode::OK);

        let performer = app.user("Performer", "5678").unwrap();
        assert!(performer.is_some());
        let performer_login = login(&app, performer.unwrap());
        let performer_cookie = performer_login
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let forbidden = call(
            &app,
            Request::get("/api/admin/ping")
                .header(header::COOKIE, performer_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        assert!(response_text(forbidden)
            .await
            .contains("Sign in as an admin"));
    }

    #[tokio::test]
    async fn deleting_or_resetting_a_user_invalidates_its_session() {
        let app = App::memory().unwrap();
        app.add_user("Performer", "5678", "performer").unwrap();
        let response = login(&app, app.user("Performer", "5678").unwrap().unwrap());
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        app.db
            .lock()
            .unwrap()
            .execute("DELETE FROM users WHERE name=?1", ["Performer"])
            .unwrap();
        let signed_out = call(
            &app,
            Request::get("/api/bootstrap")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(signed_out.status(), StatusCode::OK);
        assert!(response_text(signed_out).await.contains("\"user\":null"));
    }

    #[tokio::test]
    async fn session_survives_a_server_restart() {
        let path = std::env::temp_dir().join(format!(
            "cl5-session-restart-{}-{}.sqlite",
            std::process::id(),
            rand::random::<u64>()
        ));
        let app = App::open(path.to_str().unwrap()).unwrap();
        app.add_user("Performer", "5678", "performer").unwrap();
        let response = login(&app, app.user("Performer", "5678").unwrap().unwrap());
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        drop(app);

        let restarted = App::open(path.to_str().unwrap()).unwrap();
        let home = call(
            &restarted,
            Request::get("/api/bootstrap")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(home.status(), StatusCode::OK);
        assert!(response_text(home).await.contains("\"name\":\"Performer\""));
        drop(restarted);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn signed_in_display_name_is_returned_as_json_data() {
        let app = App::memory().unwrap();
        let name = "<svg onload=alert(1)>";
        app.add_user(name, "1234", "admin").unwrap();
        let login_response = login(&app, app.user(name, "1234").unwrap().unwrap());
        let cookie = login_response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let home = call(
            &app,
            Request::get("/api/bootstrap")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            home.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = response_text(home).await;
        assert!(body.contains("<svg onload=alert(1)>"));
    }

    #[tokio::test]
    async fn bootstrap_offers_existing_names_for_sign_in() {
        let app = App::memory().unwrap();
        app.add_user("Zoe", "1234", "performer").unwrap();
        app.add_user("Alex", "5678", "performer").unwrap();

        let response = call(
            &app,
            Request::get("/api/bootstrap").body(Body::empty()).unwrap(),
        )
        .await;

        assert!(response_text(response)
            .await
            .contains("\"users\":[\"Alex\",\"Zoe\"]"));
    }

    #[tokio::test]
    async fn admin_user_actions_audit_and_invalidate_sessions() {
        let app = App::memory().unwrap();
        app.add_user("Corey", "1234", "admin").unwrap();
        let created = app
            .create_user(
                "Corey",
                &NewUser {
                    name: "Sam".into(),
                    role: "performer".into(),
                    passcode: "Generate".into(),
                },
            )
            .unwrap();
        assert!(created.bytes().all(|byte| byte.is_ascii_digit()));
        assert_eq!(created.len(), 4);
        assert!(app
            .create_user(
                "Corey",
                &NewUser {
                    name: "Sam".into(),
                    role: "performer".into(),
                    passcode: "1234".into(),
                },
            )
            .unwrap_err()
            .contains("already"));
        app.assign_mixes("Corey", "Sam", &["1".into(), "2".into()])
            .unwrap();
        let login_response = login(&app, app.user("Sam", &created).unwrap().unwrap());
        let cookie = login_response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let reset = app.reset_passcode("Corey", "Sam", "4321").unwrap();
        assert_eq!(reset, "4321");
        let signed_out = call(
            &app,
            Request::get("/api/bootstrap")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(response_text(signed_out).await.contains("\"user\":null"));
        let overview = app.admin_overview().await.unwrap();
        let sam = overview
            .users
            .iter()
            .find(|user| user.name == "Sam")
            .unwrap();
        assert_eq!(sam.mixes, ["Mix 1", "Mix 2"]);
        assert!(app.assign_mixes("Corey", "Sam", &["25".into()]).is_err());
        assert!(overview
            .history
            .iter()
            .any(|row| row.what.contains("Reset")));
        app.delete_user("Corey", "Sam").unwrap();
        assert!(app.user("Sam", "4321").unwrap().is_none());
        assert!(app
            .delete_user("Corey", "Corey")
            .unwrap_err()
            .contains("yourself"));
    }
}
