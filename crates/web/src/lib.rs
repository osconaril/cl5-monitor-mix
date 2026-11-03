//! CL5 Monitor Mix's browser client, compiled to WebAssembly by Trunk.

use gloo_net::http::Request;
use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;
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

#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Loading,
    Setup,
    SignIn(Vec<String>),
    SignedIn(User),
    Unavailable,
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
        Screen::SignedIn(user) => html! {
            <>
                <h1>{format!("Welcome, {}", user.name)}</h1>
                <p>{format!("Signed in as {}.", user.role)}</p>
                <form method="post" action="/signout"><button>{"Sign out"}</button></form>
            </>
        },
    }
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
    use super::error_notice;

    #[test]
    fn lockout_notice_uses_the_server_reported_time_left() {
        assert_eq!(
            error_notice("?error=locked&seconds=137"),
            Some("This name is locked. Try again in 137 seconds.".into())
        );
    }
}
