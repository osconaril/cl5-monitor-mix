//! Offline LAN sign-in server. Console control is intentionally a later card.
use axum::{
    extract::{Form, State},
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
    collections::HashMap,
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
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

#[tokio::main]
async fn main() {
    let path = env::var("CL5_DATABASE_PATH").unwrap_or_else(|_| "cl5-monitor-mix.sqlite".into());
    let address: SocketAddr = env::var("CL5_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .expect("CL5_LISTEN must be host:port");
    let app = App::open(&path).expect("open SQLite database");
    if !app.has_admin().expect("read users") {
        println!(
            "First-run setup: open http://<this-show-machine's-LAN-IP>:{} /setup from the stage network.",
            address.port()
        );
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
        .route("/", get(web_app))
        .route("/setup", get(web_app).post(setup))
        .route("/signin", post(signin))
        .route("/signout", post(signout))
        .route("/api/admin/ping", get(admin_ping))
        .route("/api/bootstrap", get(bootstrap))
        .with_state(app)
        .fallback_service(ServeDir::new(web_dist()))
}
impl App {
    fn open(path: &str) -> rusqlite::Result<Self> {
        let c = Connection::open(path)?;
        migrate(&c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
            attempts: Default::default(),
        })
    }
    #[cfg(test)]
    fn memory() -> rusqlite::Result<Self> {
        let c = Connection::open_in_memory()?;
        migrate(&c)?;
        Ok(Self {
            db: Arc::new(Mutex::new(c)),
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
    })
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
    a.db.lock()
        .unwrap()
        .execute(
            "INSERT INTO sessions(token,user_name,user_updated_at,created_at) VALUES(?1,?2,?3,?4)",
            params![&t, &u.name, &u.updated_at, Utc::now().to_rfc3339()],
        )
        .expect("store session");
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
}
