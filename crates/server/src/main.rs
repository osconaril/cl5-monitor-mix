//! Offline LAN sign-in server. Console control is intentionally a later card.
use axum::{
    extract::{Form, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use rand::{distr::Alphanumeric, Rng};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Db = Arc<Mutex<Connection>>;
#[derive(Clone)]
struct App {
    db: Db,
    sessions: Arc<Mutex<HashMap<String, User>>>,
    attempts: Arc<Mutex<HashMap<String, Attempt>>>,
}
#[derive(Clone)]
struct User {
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

#[tokio::main]
async fn main() {
    let path = env::var("CL5_DATABASE_PATH").unwrap_or_else(|_| "cl5-monitor-mix.sqlite".into());
    let address: SocketAddr = env::var("CL5_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .expect("CL5_LISTEN must be host:port");
    let app = App::open(&path).expect("open SQLite database");
    if !app.has_admin().expect("read users") {
        println!("First-run setup: http://{address}/setup");
    }
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .expect("bind CL5_LISTEN");
    println!("CL5 Monitor Mix listening on http://{address}");
    axum::serve(listener, routes(app))
        .await
        .expect("serve HTTP");
}
fn routes(app: App) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/setup", get(setup_page).post(setup))
        .route("/signin", post(signin))
        .route("/signout", post(signout))
        .route("/api/admin/ping", get(admin_ping))
        .with_state(app)
}
impl App {
    fn open(path: &str) -> rusqlite::Result<Self> {
        let c = Connection::open(path)?;
        migrate(&c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            sessions: Default::default(),
            attempts: Default::default(),
        })
    }
    #[cfg(test)]
    fn memory() -> rusqlite::Result<Self> {
        let c = Connection::open_in_memory()?;
        migrate(&c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            sessions: Default::default(),
            attempts: Default::default(),
        })
    }
    fn has_admin(&self) -> rusqlite::Result<bool> {
        Ok(self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE role='admin')",
            [],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }
    fn add_user(&self, n: &str, p: &str, role: &str) -> Result<(), String> {
        valid(n, p)?;
        let now = Utc::now().to_rfc3339();
        self.db.lock().unwrap().execute("INSERT INTO users(name,passcode,role,created_at,updated_at) VALUES(?1,?2,?3,?4,?4)",params![n.trim(),p,role,now]).map(|_|()).map_err(|_|"That name is already in use.".into())
    }
    fn user(&self, n: &str, p: &str) -> rusqlite::Result<Option<User>> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT name,role FROM users WHERE name=?1 AND passcode=?2",
                params![n.trim(), p],
                |r| {
                    Ok(User {
                        name: r.get(0)?,
                        role: r.get(1)?,
                    })
                },
            )
            .optional()
    }
}
fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(include_str!("../migrations/0001_users.sql"))?;
    c.execute(
        "INSERT OR IGNORE INTO schema_migrations VALUES('0001_users',?1)",
        [Utc::now().to_rfc3339()],
    )?;
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
fn page(title: &str, body: &str) -> Html<String> {
    Html(format!("<!doctype html><meta name=viewport content='width=device-width,initial-scale=1'><title>{title} · Monitor Mix</title><style>body{{font:18px system-ui;background:#111;color:#fff;max-width:34rem;margin:auto;padding:1rem}}main{{display:grid;gap:1rem}}input,button{{font:inherit;min-height:44px;padding:.5rem}}button{{background:#4cf;border:0;border-radius:.4rem}}@media(orientation:landscape){{body{{max-width:48rem}}}}</style><main>{body}</main>"))
}
async fn home(State(a): State<App>, h: HeaderMap) -> Response {
    if let Some(u) = session(&a, &h) {
        page("Monitor Mix",&format!("<h1>Welcome, {}</h1><p>Signed in as {}.</p><form method=post action=/signout><button>Sign out</button></form>",u.name,u.role)).into_response()
    } else if a.has_admin().unwrap_or(false) {
        page("Sign in","<h1>Monitor Mix</h1><form method=post action=/signin><label>Name <input name=name required></label><label>4-digit passcode <input name=passcode inputmode=numeric pattern='[0-9]{4}' maxlength=4 required></label><button>Sign in</button></form>").into_response()
    } else {
        Redirect::to("/setup").into_response()
    }
}
async fn setup_page(State(a): State<App>) -> Response {
    if a.has_admin().unwrap_or(true) {
        Redirect::to("/").into_response()
    } else {
        page("First setup","<h1>Create the first admin</h1><p>This page works only once.</p><form method=post action=/setup><label>Name <input name=name required></label><label>4-digit passcode <input name=passcode inputmode=numeric pattern='[0-9]{4}' maxlength=4 required></label><button>Create admin</button></form>").into_response()
    }
}
async fn setup(State(a): State<App>, Form(f): Form<Credentials>) -> Response {
    if a.has_admin().unwrap_or(true) {
        return Redirect::to("/").into_response();
    }
    match a.add_user(&f.name, &f.passcode, "admin") {
        Ok(()) => login(&a, a.user(&f.name, &f.passcode).unwrap().unwrap()),
        Err(e) => page(
            "First setup",
            &format!("<p>{e}</p><a href=/setup>Try again</a>"),
        )
        .into_response(),
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
            return page(
                "Sign in",
                &format!(
                    "<p>This name is locked. Try again in {} seconds.</p>",
                    until.duration_since(Instant::now()).as_secs()
                ),
            )
            .into_response();
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
                e.locked_until = Some(Instant::now() + Duration::from_secs(300));
            }
            page("Sign in", "<p>Wrong passcode.</p><a href=/>Try again</a>").into_response()
        }
    }
}
async fn signout(State(a): State<App>, h: HeaderMap) -> Response {
    if let Some(t) = cookie(&h) {
        a.sessions.lock().unwrap().remove(t);
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
fn login(a: &App, u: User) -> Response {
    let t: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    a.sessions.lock().unwrap().insert(t.clone(), u);
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
fn cookie(h: &HeaderMap) -> Option<&str> {
    h.get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|x| x.strip_prefix("cl5_session="))
}
fn session(a: &App, h: &HeaderMap) -> Option<User> {
    a.sessions.lock().unwrap().get(cookie(h)?).cloned()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn database_migrates_and_accepts_admin() {
        let a = App::memory().unwrap();
        assert!(!a.has_admin().unwrap());
        a.add_user("Corey", "1234", "admin").unwrap();
        assert!(a.has_admin().unwrap());
        assert_eq!(a.user("Corey", "1234").unwrap().unwrap().role, "admin");
    }
    #[test]
    fn passcodes_are_four_digits() {
        assert!(valid("a", "1234").is_ok());
        assert!(valid("a", "123").is_err());
        assert!(valid("a", "abcd").is_err());
    }
}
