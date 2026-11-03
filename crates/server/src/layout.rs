//! Per-user presentation settings. None of these writes touch the console.
use crate::{session, App};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub order: Vec<u8>,
    pub hidden: Vec<u8>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub channels: Vec<u8>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            order: (0..80).collect(),
            hidden: Vec::new(),
            groups: Vec::new(),
        }
    }
}

impl Layout {
    fn validate(&self) -> Result<(), &'static str> {
        fn valid_channels(channels: &[u8]) -> bool {
            channels.len() <= 80
                && channels.iter().all(|channel| *channel < 80)
                && channels.iter().collect::<HashSet<_>>().len() == channels.len()
        }
        if self.order.len() != 80 || !valid_channels(&self.order) {
            return Err("Channel order must include every channel exactly once.");
        }
        if !valid_channels(&self.hidden) {
            return Err("Hidden channels must be distinct channel numbers from 1 through 80.");
        }
        if self.groups.len() > 80 {
            return Err("You can save up to 80 groups.");
        }
        let mut ids = HashSet::new();
        for group in &self.groups {
            if group.id.is_empty() || group.id.len() > 64 || !ids.insert(&group.id) {
                return Err("Each group needs a distinct identifier.");
            }
            if group.name.trim().is_empty() || group.name.chars().count() > 80 {
                return Err("Give each group a name of 1 to 80 characters.");
            }
            if group.channels.is_empty() || !valid_channels(&group.channels) {
                return Err("Choose distinct channels from 1 through 80 for each group.");
            }
        }
        Ok(())
    }
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(user) = session(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "Sign in first.").into_response();
    };
    let result = app
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT layout_json FROM performer_layouts WHERE user_name=?1",
            [&user.name],
            |row| row.get::<_, String>(0),
        )
        .optional();
    match result {
        Ok(None) => Json(Layout::default()).into_response(),
        Ok(Some(json)) => match serde_json::from_str::<Layout>(&json) {
            Ok(layout) => Json(layout).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load your saved layout.",
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not load your saved layout.",
        )
            .into_response(),
    }
}

pub async fn save(
    State(app): State<App>,
    headers: HeaderMap,
    Json(layout): Json<Layout>,
) -> Response {
    let Some(user) = session(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "Sign in first.").into_response();
    };
    if let Err(message) = layout.validate() {
        return (StatusCode::BAD_REQUEST, message).into_response();
    }
    // A single statement replaces the complete layout atomically. The owner is
    // always the session user, never an identifier supplied by the browser.
    let result = app.db.lock().unwrap().execute(
        "INSERT INTO performer_layouts(user_name,layout_json) VALUES(?1,?2)
         ON CONFLICT(user_name) DO UPDATE SET layout_json=excluded.layout_json",
        params![
            user.name,
            serde_json::to_string(&layout).expect("serializable layout")
        ],
    );
    match result {
        Ok(_) => Json(layout).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not save your layout. Try again.",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        login, routes,
        tests::{call, response_text},
    };
    use axum::{
        body::Body,
        http::{header, Request},
    };
    use tower::ServiceExt;

    fn cookie(app: &App, name: &str) -> String {
        login(app, app.user(name, "1234").unwrap().unwrap()).headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    fn request(cookie: &str, layout: Option<&Layout>) -> Request<Body> {
        let builder = Request::builder()
            .uri("/api/performer/layout")
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json");
        match layout {
            Some(layout) => builder
                .method("PUT")
                .body(Body::from(serde_json::to_vec(layout).unwrap()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    fn customized() -> Layout {
        let mut layout = Layout::default();
        layout.order.swap(0, 39);
        layout.hidden = vec![2, 79];
        layout.groups = vec![Group {
            id: "drums".into(),
            name: "Drums".into(),
            channels: vec![0, 1, 2],
        }];
        layout
    }

    #[tokio::test]
    async fn layout_is_per_user_and_returns_on_another_sign_in() {
        let app = App::memory().unwrap();
        for name in ["Singer", "Drummer"] {
            app.add_user(name, "1234", "performer").unwrap();
        }
        let layout = customized();
        let saved = call(&app, request(&cookie(&app, "Singer"), Some(&layout))).await;
        assert_eq!(saved.status(), StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(saved).await).unwrap(),
            layout
        );
        let second_device = call(&app, request(&cookie(&app, "Singer"), None)).await;
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(second_device).await).unwrap(),
            layout
        );
        let other = call(&app, request(&cookie(&app, "Drummer"), None)).await;
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(other).await).unwrap(),
            Layout::default()
        );

        let mut edited = layout.clone();
        edited.groups[0].name = "Band".into();
        edited.hidden.clear();
        assert_eq!(
            call(&app, request(&cookie(&app, "Singer"), Some(&edited)))
                .await
                .status(),
            StatusCode::OK
        );
        let readback = call(&app, request(&cookie(&app, "Singer"), None)).await;
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(readback).await).unwrap(),
            edited
        );
        edited.groups.clear();
        assert_eq!(
            call(&app, request(&cookie(&app, "Singer"), Some(&edited)))
                .await
                .status(),
            StatusCode::OK
        );
        let readback = call(&app, request(&cookie(&app, "Singer"), None)).await;
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(readback).await).unwrap(),
            edited
        );
        assert!(
            !app.bridge.is_connected(),
            "layout edits work without any console writes"
        );
    }

    #[tokio::test]
    async fn layout_requires_a_session_and_invalid_updates_preserve_saved_data() {
        let app = App::memory().unwrap();
        let layout = customized();
        assert_eq!(
            call(&app, request("", None)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, request("", Some(&layout))).await.status(),
            StatusCode::UNAUTHORIZED
        );
        app.add_user("Singer", "1234", "performer").unwrap();
        let cookie = cookie(&app, "Singer");
        assert_eq!(
            call(&app, request(&cookie, Some(&layout))).await.status(),
            StatusCode::OK
        );
        let mut invalid = layout.clone();
        invalid.order[0] = invalid.order[1];
        let response = call(&app, request(&cookie, Some(&invalid))).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response_text(response)
            .await
            .contains("every channel exactly once"));
        let readback = call(&app, request(&cookie, None)).await;
        assert_eq!(
            serde_json::from_str::<Layout>(&response_text(readback).await).unwrap(),
            layout
        );
        let mut spoofed = serde_json::to_value(&layout).unwrap();
        spoofed["user_name"] = "Someone else".into();
        let response = call(
            &app,
            Request::put("/api/performer/layout")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(spoofed.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        app.reset_passcode("Admin", "Singer", "5678").unwrap();
        assert_eq!(
            call(&app, request(&cookie, None)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&app, request(&cookie, Some(&layout))).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn upgrade_preserves_existing_users_and_assignments_and_is_repeatable() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(include_str!("../migrations/0001_users.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0002_admin_portal.sql"))
            .unwrap();
        connection.execute_batch(
            "INSERT INTO schema_migrations VALUES('0001_users','before'),('0002_admin_portal','before');
             INSERT INTO users(name,passcode,role,created_at,updated_at) VALUES('Singer','1234','performer','before','before');
             INSERT INTO user_mixes VALUES('Singer','Mix 3');"
        ).unwrap();
        crate::migrate(&mut connection).unwrap();
        crate::migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT mix_name FROM user_mixes WHERE user_name='Singer'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "Mix 3"
        );
        connection
            .execute(
                "INSERT INTO performer_layouts VALUES('Singer',?1)",
                [serde_json::to_string(&customized()).unwrap()],
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version='0003_performer_layouts'",
                    [],
                    |row| row.get::<_, u32>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn rejects_invalid_channel_sets_and_groups() {
        let mut layout = customized();
        layout.hidden = vec![80];
        assert!(layout.validate().is_err());
        layout.hidden = vec![1, 1];
        assert!(layout.validate().is_err());
        layout.hidden.clear();
        layout.groups[0].channels = vec![80];
        assert!(layout.validate().is_err());
        layout.groups[0].channels = vec![1, 1];
        assert!(layout.validate().is_err());
        layout.groups[0].channels.clear();
        assert!(layout.validate().is_err());
        layout.groups[0].channels = vec![1];
        layout.groups[0].name = "  ".into();
        assert!(layout.validate().is_err());
        layout.groups[0].name = "Drums".into();
        layout.groups.push(layout.groups[0].clone());
        assert!(layout.validate().is_err());
    }

    #[tokio::test]
    async fn layout_survives_database_reopen_but_not_user_deletion() {
        let path =
            std::env::temp_dir().join(format!("cl5-layout-{}.sqlite", rand::random::<u64>()));
        let layout = customized();
        {
            let app = App::open(path.to_str().unwrap()).unwrap();
            app.add_user("Admin", "1234", "admin").unwrap();
            app.add_user("Singer", "1234", "performer").unwrap();
            assert_eq!(
                call(&app, request(&cookie(&app, "Singer"), Some(&layout)))
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        {
            let app = App::open(path.to_str().unwrap()).unwrap();
            let response = routes(app.clone())
                .oneshot(request(&cookie(&app, "Singer"), None))
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Layout>(&response_text(response).await).unwrap(),
                layout
            );
            app.delete_user("Admin", "Singer").unwrap();
            app.add_user("Singer", "1234", "performer").unwrap();
            let response = call(&app, request(&cookie(&app, "Singer"), None)).await;
            assert_eq!(
                serde_json::from_str::<Layout>(&response_text(response).await).unwrap(),
                Layout::default()
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
