//! CL5 Monitor Mix's browser client, compiled to WebAssembly by Trunk.

use gloo_net::http::Request;
use gloo_timers::callback::{Interval, Timeout};
use serde::Deserialize;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::spawn_local;
use web_sys::{MessageEvent, WebSocket};
use yew::prelude::*;

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct User {
    name: String,
    role: String,
}

#[derive(Debug, Deserialize)]
struct Bootstrap {
    has_admin: bool,
    user: Option<User>,
    users: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct AdminUser {
    name: String,
    role: String,
    passcode: String,
    mixes: Vec<String>,
    last_sign_in: Option<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct HistoryRow {
    who: String,
    what: String,
    when: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct ConsoleStatus {
    connected: bool,
    address: String,
    scene_number: Option<u16>,
    scene_name: Option<String>,
    phones_connected: usize,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct AdminOverview {
    users: Vec<AdminUser>,
    history: Vec<HistoryRow>,
    console: ConsoleStatus,
}
#[derive(Clone, Debug, Deserialize, PartialEq)]
struct PerformerMixes {
    mixes: Vec<String>,
}

fn refresh_performer_mixes(mixes: UseStateHandle<Option<PerformerMixes>>) {
    spawn_local(async move {
        if let Ok(response) = Request::get("/api/performer/mixes").send().await {
            mixes.set(response.json::<PerformerMixes>().await.ok());
        }
    });
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BridgeMessage {
    Snapshot { kind: String, connected: bool },
    Event(BridgeEvent),
}

#[derive(Debug, Deserialize)]
enum BridgeEvent {
    Connected,
    Disconnected,
    Value { key: String, value: String },
    SceneRecalled { number: u16, name: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Loading,
    Setup,
    SignIn(Vec<String>),
    SignedIn(User),
    Unavailable,
}

const SCENE_RECALL_NOTICE_MS: u32 = 3_000;

fn connection_notice(connected: bool) -> &'static str {
    if connected {
        "Console connected"
    } else {
        "Console disconnected — reconnecting"
    }
}

/// The sign-in and first-run UI. Later cards extend this component rather than
/// adding server-side HTML templates.
#[function_component(App)]
pub fn app() -> Html {
    let screen = use_state(|| Screen::Loading);
    let notice = web_sys::window()
        .and_then(|window| window.location().search().ok())
        .and_then(|search| error_notice(&search));
    {
        let screen = screen.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                let next = match Request::get("/api/bootstrap").send().await {
                    Ok(response) => response.json::<Bootstrap>().await.ok(),
                    Err(_) => None,
                }
                .map(|bootstrap| match bootstrap.user {
                    Some(user) => Screen::SignedIn(user),
                    None if bootstrap.has_admin => Screen::SignIn(bootstrap.users),
                    None => Screen::Setup,
                })
                .unwrap_or(Screen::Unavailable);
                screen.set(next);
            });
            || ()
        });
    }

    match &*screen {
        Screen::Loading => html! { <p role="status">{"Loading Monitor Mix…"}</p> },
        Screen::Unavailable => html! {
            <><h1>{"Monitor Mix"}</h1><p>{"The show-machine server could not be reached. Check the LAN connection and try again."}</p></>
        },
        Screen::Setup => html! {
            <>
                <h1>{"Create the first admin"}</h1>
                <p>{"This page works only once."}</p>
                {notice.map(|text| html! { <p role="alert">{text}</p> }).unwrap_or_default()}
                <form method="post" action="/setup">
                    <label>{"Name"}<input name="name" required=true /></label>
                    <label>{"4-digit passcode"}<input name="passcode" inputmode="numeric" pattern="[0-9]{4}" maxlength="4" required=true /></label>
                    <button>{"Create admin"}</button>
                </form>
            </>
        },
        Screen::SignIn(users) => html! {
            <>
                <h1>{"Monitor Mix"}</h1>
                {notice.map(|text| html! { <p role="alert">{text}</p> }).unwrap_or_default()}
                <form method="post" action="/signin">
                    <label>{"Name"}<input name="name" list="existing-user-names" required=true /></label>
                    <datalist id="existing-user-names">
                        {for users.iter().map(|name| html! { <option value={name.clone()} /> })}
                    </datalist>
                    <p>{"Choose a saved name from the list or type your name."}</p>
                    <label>{"4-digit passcode"}<input name="passcode" inputmode="numeric" pattern="[0-9]{4}" maxlength="4" required=true /></label>
                    <button>{"Sign in"}</button>
                </form>
            </>
        },
        Screen::SignedIn(user) => {
            if user.role == "admin" {
                html! { <AdminPortal user={user.clone()} /> }
            } else {
                html! { <><PerformerAssignments user={user.clone()} /><LiveConsole /></> }
            }
        }
    }
}

#[derive(Properties, PartialEq)]
struct PerformerAssignmentsProps {
    user: User,
}

#[function_component(PerformerAssignments)]
fn performer_assignments(props: &PerformerAssignmentsProps) -> Html {
    let mixes = use_state(|| None::<PerformerMixes>);
    {
        let mixes = mixes.clone();
        use_effect_with((), move |_| {
            refresh_performer_mixes(mixes.clone());
            let interval = Interval::new(1_000, move || refresh_performer_mixes(mixes.clone()));
            move || drop(interval)
        });
    }
    let assignment = match &*mixes {
        Some(value) if value.mixes.is_empty() => "No mixes assigned yet.".into(),
        Some(value) => format!("Assigned mix numbers: {}", value.mixes.join(", ")),
        None => "Loading your mix assignments…".into(),
    };
    html! { <><h1>{format!("Welcome, {}", props.user.name)}</h1><p>{assignment}</p><p>{"Assignments update automatically."}</p><form method="post" action="/signout"><button>{"Sign out"}</button></form></> }
}

#[derive(Properties, PartialEq)]
struct AdminPortalProps {
    user: User,
}

#[function_component(AdminPortal)]
fn admin_portal(props: &AdminPortalProps) -> Html {
    let overview = use_state(|| None::<AdminOverview>);
    {
        let overview = overview.clone();
        use_effect_with((), move |_| {
            let refresh = move || {
                let overview = overview.clone();
                spawn_local(async move {
                    if let Ok(response) = Request::get("/api/admin/overview").send().await {
                        overview.set(response.json::<AdminOverview>().await.ok());
                    }
                });
            };
            refresh();
            let interval = Interval::new(1_000, refresh);
            move || drop(interval)
        });
    }
    let Some(overview) = &*overview else {
        return html! { <p role="status">{"Loading admin portal…"}</p> };
    };
    let scene = match (&overview.console.scene_number, &overview.console.scene_name) {
        (Some(_number), Some(name)) if !name.is_empty() => format!("{name} · Monitor Mix"),
        (Some(number), _) => format!("Scene {number} · Monitor Mix"),
        _ => "Console disconnected — scene unknown".into(),
    };
    if let Some(window) = web_sys::window() {
        document_title(&window, &scene);
    }
    html! {
        <>
            <header class="scene-bar"><small>{"Console"}</small><strong>{scene.clone()}</strong></header>
            <h1>{"Users"}</h1>
            <p>{format!("Signed in as {}. Console {} at {} · {} phone{} connected.", props.user.name, if overview.console.connected { "connected" } else { "disconnected" }, overview.console.address, overview.console.phones_connected, if overview.console.phones_connected == 1 { "" } else { "s" })}</p>
            <section class="panel"><h2>{"Add user"}</h2><form method="post" action="/api/admin/users"><label>{"Name"}<input name="name" required=true /></label><label>{"Role"}<select name="role"><option value="performer">{"Performer"}</option><option value="admin">{"Admin"}</option></select></label><label>{"4-digit passcode"}<input name="passcode" value="Generate" pattern="([0-9]{4}|Generate)" required=true /></label><button>{"Add user"}</button></form></section>
            <section class="users"><h2>{"Saved users"}</h2>{for overview.users.iter().map(user_card)}</section>
            <section class="panel"><h2>{"History"}</h2><ul class="history">{for overview.history.iter().map(|row| html! { <li><strong>{&row.who}</strong>{": "}{&row.what}<small>{format!(" · {}", row.when)}</small></li> })}</ul></section>
            <form method="post" action="/signout"><button>{"Sign out"}</button></form>
        </>
    }
}

fn user_card(user: &AdminUser) -> Html {
    let encoded = user.name.replace(' ', "%20");
    let mixes = if user.mixes.is_empty() {
        "No mixes assigned".into()
    } else {
        user.mixes.join(", ")
    };
    html! { <article class="user-card"><h3>{&user.name}</h3><p>{format!("{} · code {}", user.role, user.passcode)}</p><p>{format!("Mixes: {mixes}")}</p><p>{format!("Last sign-in: {}", user.last_sign_in.as_deref().unwrap_or("Never"))}</p><form method="post" action={format!("/api/admin/users/{encoded}/passcode")}><label>{"New passcode"}<input name="passcode" value="Generate" pattern="([0-9]{4}|Generate)" required=true /></label><button>{"Reset passcode"}</button></form><form method="post" action={format!("/api/admin/users/{encoded}/mixes")}><fieldset><legend>{"Mix numbers"}</legend>{for (1..=24).map(|number| { let selected = user.mixes.contains(&format!("Mix {number}")); html! { <label class="mix-choice"><input type="checkbox" name="mixes" value={number.to_string()} checked={selected} />{format!("Mix {number}")}</label> } })}</fieldset><button>{"Save mix assignments"}</button></form><form method="post" action={format!("/api/admin/users/{encoded}")}><p class="delete-note">{format!("Delete {}? Their saved settings go too.", user.name)}</p><button>{"Delete user"}</button></form></article> }
}

fn document_title(window: &web_sys::Window, title: &str) {
    if let Some(document) = window.document() {
        document.set_title(title);
    }
}

/// Keeps every already-open screen subscribed to the server-owned bridge.
/// Mix controls are deliberately left to CL5-6; this card supplies the shared
/// connection, state notifications, and operator-visible safety notices.
#[function_component(LiveConsole)]
fn live_console() -> Html {
    let notice = use_state(|| "Connecting to console…".to_owned());
    let connected = Rc::new(Cell::new(false));
    let scene_notice_timeout = Rc::new(RefCell::new(None::<Timeout>));
    {
        let notice = notice.clone();
        let connected = connected.clone();
        let scene_notice_timeout = scene_notice_timeout.clone();
        use_effect_with((), move |_| -> Box<dyn FnOnce()> {
            let Some(window) = web_sys::window() else {
                return Box::new(|| ());
            };
            let location = window.location();
            let protocol = match location.protocol().ok().as_deref() {
                Some("https:") => "wss:",
                _ => "ws:",
            };
            let Ok(host) = location.host() else {
                return Box::new(|| ());
            };
            let Ok(socket) = WebSocket::new(&format!("{protocol}//{host}/api/console")) else {
                notice.set("Console disconnected — reconnecting".into());
                return Box::new(|| ());
            };
            let message_notice = notice.clone();
            let message_connected = connected.clone();
            let message_scene_notice_timeout = scene_notice_timeout.clone();
            let callback = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    return;
                };
                match serde_json::from_str::<BridgeMessage>(&text) {
                    Ok(BridgeMessage::Snapshot { kind, connected })
                        if kind == "snapshot" && connected =>
                    {
                        message_connected.set(true);
                        message_scene_notice_timeout.borrow_mut().take();
                        message_notice.set(connection_notice(true).into());
                    }
                    Ok(BridgeMessage::Snapshot { .. })
                    | Ok(BridgeMessage::Event(BridgeEvent::Disconnected)) => {
                        message_connected.set(false);
                        message_scene_notice_timeout.borrow_mut().take();
                        message_notice.set(connection_notice(false).into());
                    }
                    Ok(BridgeMessage::Event(BridgeEvent::Connected)) => {
                        message_connected.set(true);
                        message_scene_notice_timeout.borrow_mut().take();
                        message_notice.set(connection_notice(true).into());
                    }
                    Ok(BridgeMessage::Event(BridgeEvent::SceneRecalled { number, name })) => {
                        let name = name.unwrap_or_else(|| "Unnamed".into());
                        message_notice.set(format!("Scene {number} {name} recalled — mix updated"));
                        let timeout_notice = message_notice.clone();
                        let timeout_connected = message_connected.clone();
                        *message_scene_notice_timeout.borrow_mut() =
                            Some(Timeout::new(SCENE_RECALL_NOTICE_MS, move || {
                                timeout_notice
                                    .set(connection_notice(timeout_connected.get()).into())
                            }));
                    }
                    Ok(BridgeMessage::Event(BridgeEvent::Value { key, value })) => {
                        let _ = (key, value); // CL5-6 renders the shared live values as controls.
                    }
                    _ => {}
                }
            });
            socket.set_onmessage(Some(callback.as_ref().unchecked_ref()));
            Box::new(move || {
                socket.set_onmessage(None);
                scene_notice_timeout.borrow_mut().take();
                let _ = socket.close();
                drop(callback);
            })
        });
    }
    html! { <p role="status">{(*notice).clone()}</p> }
}

fn error_notice(search: &str) -> Option<String> {
    if search.contains("error=locked") {
        let seconds = search
            .split('&')
            .find_map(|part| part.strip_prefix("seconds=")?.parse::<u64>().ok())?;
        Some(format!(
            "This name is locked. Try again in {seconds} seconds."
        ))
    } else if search.contains("error=wrong") {
        Some("Wrong passcode. Check the 4 digits and try again.".into())
    } else if search.contains("error=setup") {
        Some("The first admin could not be created. Check the details and try again.".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{connection_notice, error_notice, SCENE_RECALL_NOTICE_MS};

    #[test]
    fn lockout_notice_uses_the_server_reported_time_left() {
        assert_eq!(
            error_notice("?error=locked&seconds=137"),
            Some("This name is locked. Try again in 137 seconds.".into())
        );
    }

    #[test]
    fn scene_recall_notice_expires_to_current_connection_status() {
        assert_eq!(SCENE_RECALL_NOTICE_MS, 3_000);
        assert_eq!(connection_notice(true), "Console connected");
        assert_eq!(
            connection_notice(false),
            "Console disconnected — reconnecting"
        );
    }
}
