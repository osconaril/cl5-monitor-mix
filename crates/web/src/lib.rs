//! CL5 Monitor Mix's browser client, compiled to WebAssembly by Trunk.

use gloo_net::http::Request;
use gloo_timers::callback::{Interval, Timeout};
use serde::Deserialize;
use std::{collections::HashMap, rc::Rc};

mod performer;
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
    mix_labels: Vec<String>,
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
    Snapshot {
        kind: String,
        connected: bool,
        state: BridgeState,
    },
    Event(BridgeEvent),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
struct BridgeState {
    scene_number: Option<u16>,
    scene_name: Option<String>,
    #[serde(default)]
    values: HashMap<String, String>,
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
                html! { <LiveConsole><performer::Performer user={user.clone()} /></LiveConsole> }
            }
        }
    }
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
    html! {
        <>
            <LiveConsole />
            <h1>{"Users"}</h1>
            <p>{format!("Signed in as {}. Console {} at {} · {} phone{} connected.", props.user.name, if overview.console.connected { "connected" } else { "disconnected" }, overview.console.address, overview.console.phones_connected, if overview.console.phones_connected == 1 { "" } else { "s" })}</p>
            <section class="panel"><h2>{"Add user"}</h2><form method="post" action="/api/admin/users"><label>{"Name"}<input name="name" required=true /></label><label>{"Role"}<select name="role"><option value="performer">{"Performer"}</option><option value="admin">{"Admin"}</option></select></label><label>{"4-digit passcode"}<input name="passcode" value="Generate" pattern="([0-9]{4}|Generate)" required=true /></label><button>{"Add user"}</button></form></section>
            <section class="users"><h2>{"Saved users"}</h2>{for overview.users.iter().cloned().map(|user| html! { <UserCard {user} mix_labels={overview.console.mix_labels.clone()} /> })}</section>
            <section class="panel"><h2>{"History"}</h2><ul class="history">{for overview.history.iter().map(|row| html! { <li><strong>{&row.who}</strong>{": "}{&row.what}<small>{format!(" · {}", row.when)}</small></li> })}</ul></section>
            <form method="post" action="/signout"><button>{"Sign out"}</button></form>
        </>
    }
}

#[derive(Properties, PartialEq)]
struct UserCardProps {
    user: AdminUser,
    mix_labels: Vec<String>,
}

#[function_component(UserCard)]
fn user_card(props: &UserCardProps) -> Html {
    let user = &props.user;
    let confirming_delete = use_state(|| false);
    // Names are data, not route syntax. Encode the whole path segment so an
    // admin can safely manage a name containing '/', '#', '?' or spaces.
    let encoded = js_sys::encode_uri_component(&user.name)
        .as_string()
        .unwrap_or_else(|| user.name.clone());
    let mixes = if user.mixes.is_empty() {
        "No mixes assigned".into()
    } else {
        user.mixes.join(", ")
    };
    let ask_to_delete = {
        let confirming_delete = confirming_delete.clone();
        Callback::from(move |_| confirming_delete.set(true))
    };
    let cancel_delete = {
        let confirming_delete = confirming_delete.clone();
        Callback::from(move |_| confirming_delete.set(false))
    };
    let delete_controls = if *confirming_delete {
        html! {
            <>
                <p class="delete-note" role="alert">{format!("Delete {}? Their saved settings go too.", user.name)}</p>
                <button>{"Confirm delete"}</button>
                <button type="button" onclick={cancel_delete}>{"Cancel"}</button>
            </>
        }
    } else {
        html! { <button type="button" onclick={ask_to_delete}>{"Delete user"}</button> }
    };
    html! { <article class="user-card"><h3>{&user.name}</h3><p>{format!("{} · code {}", user.role, user.passcode)}</p><p>{format!("Mixes: {mixes}")}</p><p>{format!("Last sign-in: {}", user.last_sign_in.as_deref().unwrap_or("Never"))}</p><form method="post" action={format!("/api/admin/users/{encoded}/passcode")}><label>{"New passcode"}<input name="passcode" value="Generate" pattern="([0-9]{4}|Generate)" required=true /></label><button>{"Reset passcode"}</button></form><form method="post" action={format!("/api/admin/users/{encoded}/mixes")}><fieldset><legend>{"Console mixes"}</legend>{for props.mix_labels.iter().enumerate().map(|(index, label)| { let number = index + 1; let selected = user.mixes.contains(&format!("Mix {number}")); html! { <label class="mix-choice"><input type="checkbox" name="mixes" value={number.to_string()} checked={selected} />{format!("Mix {number}: {label}")}</label> } })}</fieldset><button>{"Save mix assignments"}</button></form><form method="post" action={format!("/api/admin/users/{encoded}")}>{delete_controls}</form></article> }
}

fn document_title(window: &web_sys::Window, title: &str) {
    if let Some(document) = window.document() {
        document.set_title(title);
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct LiveState {
    connected: bool,
    console: BridgeState,
}

impl Reducible for LiveState {
    type Action = Vec<BridgeMessage>;

    fn reduce(self: Rc<Self>, action: Self::Action) -> Rc<Self> {
        let mut next = (*self).clone();
        for message in action {
            match message {
                BridgeMessage::Snapshot {
                    kind,
                    connected,
                    state,
                } if kind == "snapshot" => {
                    next.connected = connected;
                    next.console = state;
                }
                BridgeMessage::Event(BridgeEvent::Connected) => next.connected = true,
                BridgeMessage::Event(BridgeEvent::Disconnected) => next.connected = false,
                BridgeMessage::Event(BridgeEvent::Value { key, value }) => {
                    next.console.values.insert(key, value);
                }
                BridgeMessage::Event(BridgeEvent::SceneRecalled { number, name }) => {
                    next.console.scene_number = Some(number);
                    next.console.scene_name = name;
                }
                _ => {}
            }
        }
        Rc::new(next)
    }
}

#[derive(Properties, PartialEq)]
struct LiveConsoleProps {
    #[prop_or_default]
    children: Children,
}

/// One socket supplies the scene bar and all performer controls. Reducer actions
/// apply to the latest state, including bursts of notifications between renders.
#[function_component(LiveConsole)]
fn live_console(props: &LiveConsoleProps) -> Html {
    let live = use_reducer(LiveState::default);
    let notice = use_state(|| "Connecting to console…".to_owned());
    let retry = use_state(|| 0u32);
    {
        let live = live.clone();
        let notice = notice.clone();
        let retry = retry.clone();
        use_effect_with(*retry, move |_| -> Box<dyn FnOnce()> {
            let Some(window) = web_sys::window() else {
                return Box::new(|| ());
            };
            let protocol = if window.location().protocol().ok().as_deref() == Some("https:") {
                "wss:"
            } else {
                "ws:"
            };
            let Ok(host) = window.location().host() else {
                return Box::new(|| ());
            };
            let Ok(socket) = WebSocket::new(&format!("{protocol}//{host}/api/console")) else {
                notice.set(
                    "The show-machine connection could not be opened. Reload to try again.".into(),
                );
                return Box::new(|| ());
            };
            let timers = Rc::new(std::cell::RefCell::new(None::<Timeout>));
            let message_timers = timers.clone();
            let pending = Rc::new(std::cell::RefCell::new(Vec::new()));
            let batch_pending = pending.clone();
            let batch_live = live.clone();
            let batch = Interval::new(50, move || {
                if !batch_pending.borrow().is_empty() {
                    batch_live.dispatch(std::mem::take(&mut *batch_pending.borrow_mut()));
                }
            });
            let close_pending = pending.clone();
            let message_notice = notice.clone();
            let callback = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                let Some(text) = event.data().as_string() else {
                    return;
                };
                let Ok(message) = serde_json::from_str::<BridgeMessage>(&text) else {
                    return;
                };
                match &message {
                    BridgeMessage::Event(BridgeEvent::SceneRecalled { number, name }) => {
                        message_notice.set(format!(
                            "Scene {number} {} recalled — mix updated",
                            name.as_deref().unwrap_or("")
                        ));
                        let notice = message_notice.clone();
                        *message_timers.borrow_mut() =
                            Some(Timeout::new(SCENE_RECALL_NOTICE_MS, move || {
                                notice.set(String::new())
                            }));
                    }
                    BridgeMessage::Snapshot { connected, .. } => {
                        message_timers.borrow_mut().take();
                        message_notice.set(connection_notice(*connected).into());
                    }
                    BridgeMessage::Event(BridgeEvent::Disconnected | BridgeEvent::Connected) => {
                        message_timers.borrow_mut().take();
                        message_notice.set(
                            connection_notice(matches!(
                                message,
                                BridgeMessage::Event(BridgeEvent::Connected)
                            ))
                            .into(),
                        );
                    }
                    _ => {}
                }
                pending.borrow_mut().push(message);
            });
            socket.set_onmessage(Some(callback.as_ref().unchecked_ref()));
            let close_timers = timers.clone();
            let close = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                close_pending.borrow_mut().clear();
                live.dispatch(vec![BridgeMessage::Event(BridgeEvent::Disconnected)]);
                notice.set("Connection to the show machine lost — reconnecting.".into());
                let retry = retry.clone();
                let next = *retry + 1;
                *close_timers.borrow_mut() = Some(Timeout::new(1_000, move || retry.set(next)));
            });
            socket.set_onclose(Some(close.as_ref().unchecked_ref()));
            Box::new(move || {
                socket.set_onmessage(None);
                socket.set_onclose(None);
                timers.borrow_mut().take();
                let _ = socket.close();
                drop((callback, close, batch));
            })
        });
    }
    html! {
        <ContextProvider<LiveState> context={(*live).clone()}>
            <SceneBar connected={live.connected} scene={live.console.clone()} />
            <p role="status">{(*notice).clone()}</p>
            {props.children.clone()}
        </ContextProvider<LiveState>>
    }
}

#[derive(Properties, PartialEq)]
struct SceneBarProps {
    connected: bool,
    scene: BridgeState,
}

#[function_component(SceneBar)]
fn scene_bar(props: &SceneBarProps) -> Html {
    let (number, name, title) = match (
        props.connected,
        props.scene.scene_number,
        props.scene.scene_name.as_deref(),
    ) {
        (true, Some(number), Some(name)) if !name.is_empty() => (
            format!("Scene {number}"),
            name.to_owned(),
            format!("{name} · Monitor Mix"),
        ),
        (true, Some(number), _) => {
            let scene = format!("Scene {number}");
            (
                scene.clone(),
                scene.clone(),
                format!("{scene} · Monitor Mix"),
            )
        }
        _ => (
            "Console".into(),
            "Console disconnected — scene unknown".into(),
            "Console disconnected — scene unknown".into(),
        ),
    };
    if let Some(window) = web_sys::window() {
        document_title(&window, &title);
    }
    html! { <header class="scene-bar"><small>{number}</small><strong>{name}</strong></header> }
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
    fn live_updates_preserve_other_values_and_scene_until_replaced_by_snapshot() {
        use super::{BridgeEvent, BridgeMessage, BridgeState, LiveState};
        use std::rc::Rc;
        use yew::Reducible;
        let mut state = Rc::new(LiveState::default());
        state = state.reduce(
            [("level", "-350"), ("mute", "1"), ("level", "-351")]
                .into_iter()
                .map(|(key, value)| {
                    BridgeMessage::Event(BridgeEvent::Value {
                        key: key.into(),
                        value: value.into(),
                    })
                })
                .collect(),
        );
        state = state.reduce(vec![BridgeMessage::Event(BridgeEvent::SceneRecalled {
            number: 12,
            name: Some("Opening".into()),
        })]);
        assert_eq!(state.console.values["level"], "-351");
        assert_eq!(state.console.values["mute"], "1");
        assert_eq!(state.console.scene_number, Some(12));
        state = state.reduce(vec![BridgeMessage::Event(BridgeEvent::Connected)]);
        assert!(state.connected);
        state = state.reduce(vec![BridgeMessage::Event(BridgeEvent::Disconnected)]);
        assert!(!state.connected);
        state = state.reduce(vec![BridgeMessage::Snapshot {
            kind: "snapshot".into(),
            connected: true,
            state: BridgeState::default(),
        }]);
        assert!(state.connected);
        assert!(state.console.values.is_empty());
        assert_eq!(state.console.scene_number, None);
    }

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
