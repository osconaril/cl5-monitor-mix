//! Performer controls use console readback as the displayed source of truth.
use super::layout::{Layout, LayoutEditor};
use super::{refresh_performer_mixes, LiveState, PerformerMixes, User};
use gloo_net::http::Request;
use gloo_timers::callback::Interval;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const LEVEL: &str = "MIXER:Current/InCh/ToMix/Level";
const ON: &str = "MIXER:Current/InCh/ToMix/On";
const OFF: i16 = -32768;

fn mix_index(label: &str) -> Option<u8> {
    label
        .strip_prefix("Mix ")?
        .parse::<u8>()
        .ok()?
        .checked_sub(1)
        .filter(|n| *n < 24)
}

fn level_text(wire: i16) -> String {
    if wire == OFF {
        "Off".into()
    } else {
        format!("{} dB", f64::from(wire) / 100.0)
    }
}

/// Native range positions are dB, with one extra position for Off. Keep an
/// engineer's fractional value intact until a deliberate phone movement.
fn snap_level(wire: i16, position: f64) -> Option<String> {
    if !position.is_finite() {
        return None;
    }
    let old = if wire == OFF {
        -139.0
    } else {
        f64::from(wire) / 100.0
    };
    if position == old {
        return None;
    }
    let snapped = if wire != OFF && wire % 100 != 0 {
        if position > old {
            old.ceil()
        } else {
            old.floor()
        }
    } else {
        position.round()
    }
    .clamp(-139.0, 10.0);
    Some(if snapped < -138.0 {
        "Off".into()
    } else {
        (snapped as i16).to_string()
    })
}

#[derive(Properties, PartialEq)]
pub(super) struct PerformerProps {
    pub user: User,
}

#[function_component(Performer)]
pub(super) fn performer(props: &PerformerProps) -> Html {
    let live = use_context::<LiveState>().expect("performer has console context");
    let mixes = use_state(|| None::<PerformerMixes>);
    let selected = use_state(|| None::<String>);
    let message = use_state(String::new);
    let layout = use_state(|| None::<Layout>);
    let editing = use_state(|| false);
    let layout_error = use_state(String::new);
    let load_generation = use_state(|| 0u32);
    {
        let layout = layout.clone();
        let layout_error = layout_error.clone();
        use_effect_with(*load_generation, move |_| {
            spawn_local(async move {
                let result = async {
                    let response =
                        Request::get("/api/performer/layout")
                            .send()
                            .await
                            .map_err(|_| {
                                "Could not load your layout. Check your connection.".to_owned()
                            })?;
                    if !response.ok() {
                        return Err(response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Could not load your layout.".into()));
                    }
                    response
                        .json::<Layout>()
                        .await
                        .map_err(|_| "Could not read your layout.".to_owned())
                }
                .await;
                match result {
                    Ok(saved) => {
                        layout.set(Some(saved));
                        layout_error.set(String::new());
                    }
                    Err(error) => layout_error.set(error),
                }
            });
            || ()
        });
    }
    let channel_names: Vec<String> = (0..80)
        .map(|input| {
            live.console
                .values
                .get(&format!("MIXER:Current/InCh/Label/Name {input} 0"))
                .map(|name| name.trim_matches('"'))
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Channel {}", input + 1))
        })
        .collect();
    let shown_layout = layout.as_ref().cloned().unwrap_or_default();
    let edit_layout = {
        let editing = editing.clone();
        Callback::from(move |_| editing.set(true))
    };
    let cancel_layout = {
        let editing = editing.clone();
        Callback::from(move |_| editing.set(false))
    };
    let saved_layout = {
        let layout = layout.clone();
        let editing = editing.clone();
        Callback::from(move |saved| {
            layout.set(Some(saved));
            editing.set(false);
        })
    };
    let retry_layout = {
        let load_generation = load_generation.clone();
        Callback::from(move |_| load_generation.set(*load_generation + 1))
    };
    {
        let mixes = mixes.clone();
        use_effect_with((), move |_| {
            refresh_performer_mixes(mixes.clone());
            let interval = Interval::new(1_000, move || refresh_performer_mixes(mixes.clone()));
            move || drop(interval)
        });
    }
    let assignments = mixes.as_ref().map(|m| m.mixes.clone()).unwrap_or_default();
    // A revoked assignment immediately removes its controls. Never silently
    // retarget an old strip to a different mix.
    let active = if assignments.len() == 1 {
        assignments.first().cloned()
    } else {
        selected
            .as_ref()
            .filter(|name| assignments.contains(name))
            .cloned()
    };
    let change = {
        let message = message.clone();
        Callback::from(
            move |(mix, input, path, value): (u8, u8, &'static str, String)| {
                let message = message.clone();
                spawn_local(async move {
                    let request = Request::post("/api/console").json(&serde_json::json!({
                        "mix": mix, "input": input, "path": path, "value": value
                    }));
                    let result = match request {
                        Ok(request) => request.send().await,
                        Err(_) => {
                            message.set("Could not prepare that change. Try again.".into());
                            return;
                        }
                    };
                    match result {
                        Ok(response) if response.ok() => message.set(String::new()),
                        Ok(response) => message.set(
                            response
                                .text()
                                .await
                                .unwrap_or_else(|_| "That change was refused.".into()),
                        ),
                        Err(_) => message.set(
                            "The show machine could not be reached. Check your connection.".into(),
                        ),
                    }
                });
            },
        )
    };
    let back = {
        let selected = selected.clone();
        Callback::from(move |_| selected.set(None))
    };
    html! {
        <section class="performer">
            <h1>{format!("{}’s in-ear mix", props.user.name)}</h1>
            <p role="alert">{(*message).clone()}</p>
            if let Some(mix) = active.as_deref().and_then(mix_index) {
                <div class="mix-heading"><h2>{active.clone().unwrap_or_default()}</h2>
                if assignments.len() > 1 { <button onclick={back}>{"Choose another mix"}</button> }</div>
                if !live.connected { <p role="alert">{"Console disconnected — controls are unavailable until it reconnects."}</p> }
                <p role="alert">{(*layout_error).clone()}</p>
                if !layout_error.is_empty() { <button onclick={retry_layout}>{"Retry loading layout"}</button> }
                if *editing {
                    <LayoutEditor layout={shown_layout.clone()} names={channel_names.clone()} saved={saved_layout} cancel={cancel_layout} />
                } else {
                <button onclick={edit_layout} disabled={layout.is_none()}>{"Edit channel layout"}</button>
                <div class="channel-strips">
                {for shown_layout.order.iter().copied().filter(|input| !shown_layout.hidden.contains(input)).map(|input| {
                    let value = |path| live.console.values.get(&format!("{path} {input} {mix}"));
                    let level = value(LEVEL).and_then(|v| v.parse::<i16>().ok()).filter(|v| *v == OFF || (-13800..=1000).contains(v));
                    let on = value(ON).and_then(|v| match v.as_str() { "1" => Some(true), "0" => Some(false), _ => None });
                    let name = channel_names[input as usize].clone();
                    html! { <ChannelStrip key={format!("{mix}-{input}")} {mix} {input} {name} {level} {on} connected={live.connected} change={change.clone()} /> }
                })}
                </div>
                if !shown_layout.hidden.is_empty() {
                    <section class="hidden-channels"><h2>{"Hidden"}</h2>
                    <p>{shown_layout.hidden.iter().map(|input| channel_names[*input as usize].clone()).collect::<Vec<_>>().join(", ")}</p>
                    <p>{"Use Edit channel layout to show these channels again."}</p></section>
                }
                }
            } else {
                <p>{if mixes.is_none() { "Loading your mix assignments…" } else if assignments.is_empty() { "No mixes assigned yet. Ask the engineer to assign your mix." } else { "Choose your mix." }}</p>
                <div class="mix-picker">{for assignments.iter().filter(|name| mix_index(name).is_some()).map(|name| {
                    let selected = selected.clone();
                    let target = name.clone();
                    let onclick = Callback::from(move |_| selected.set(Some(target.clone())));
                    html! { <button {onclick}>{name}</button> }
                })}</div>
            }
            <form method="post" action="/signout"><button>{"Sign out"}</button></form>
        </section>
    }
}

#[derive(Properties, PartialEq)]
struct ChannelStripProps {
    mix: u8,
    input: u8,
    name: String,
    level: Option<i16>,
    on: Option<bool>,
    connected: bool,
    change: Callback<(u8, u8, &'static str, String)>,
}

#[function_component(ChannelStrip)]
fn channel_strip(props: &ChannelStripProps) -> Html {
    let drag_level = use_mut_ref(|| props.level);
    {
        let drag_level = drag_level.clone();
        use_effect_with(props.level, move |level| {
            *drag_level.borrow_mut() = *level;
            || ()
        });
    }
    let oninput = {
        let change = props.change.clone();
        let (mix, input) = (props.mix, props.input);
        let drag_level = drag_level.clone();
        Callback::from(move |event: InputEvent| {
            let target: web_sys::HtmlInputElement = event.target_unchecked_into();
            let wire = *drag_level.borrow();
            if let Some(wire) = wire {
                if let Some(value) = snap_level(wire, target.value_as_number()) {
                    // Snap the thumb immediately; the label remains the console's
                    // readback until the command has actually reached it.
                    target.set_value(if value == "Off" { "-139" } else { &value });
                    *drag_level.borrow_mut() = Some(if value == "Off" {
                        OFF
                    } else {
                        value.parse::<i16>().expect("snapped whole dB") * 100
                    });
                    change.emit((mix, input, LEVEL, value));
                }
            }
        })
    };
    let mute = {
        let change = props.change.clone();
        let (mix, input, on) = (props.mix, props.input, props.on);
        Callback::from(move |_| {
            if let Some(on) = on {
                change.emit((mix, input, ON, if on { "0" } else { "1" }.into()));
            }
        })
    };
    let position = props
        .level
        .map(|v| {
            if v == OFF {
                -139.0
            } else {
                f64::from(v) / 100.0
            }
        })
        .unwrap_or(-139.0);
    html! {
        <article class="channel-strip" data-channel={props.input.to_string()}>
            <h3>{&props.name}</h3>
            <label class="send-control">
                <span>{props.level.map(level_text).unwrap_or_else(|| "Level unavailable".into())}</span>
                <input type="range" aria-label={format!("{} send level", props.name)} aria-valuetext={props.level.map(level_text)}
                    min="-139" max="10" step="any" value={position.to_string()} {oninput} disabled={!props.connected || props.level.is_none()} />
            </label>
            <button onclick={mute} disabled={!props.connected || props.on.is_none()} aria-pressed={props.on.map(|on| (!on).to_string())}>
                {if props.on == Some(false) { "Muted — unmute" } else { "Mute" }}
            </button>
        </article>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_console_values_are_displayed_without_rounding() {
        assert_eq!(level_text(-350), "-3.5 dB");
        assert_eq!(level_text(-301), "-3.01 dB");
        assert_eq!(level_text(OFF), "Off");
        assert_eq!(snap_level(-350, -3.5), None);
        assert_eq!(snap_level(-350, -3.49), Some("-3".into()));
        assert_eq!(snap_level(-350, -3.51), Some("-4".into()));
        assert_eq!(snap_level(-350, -2.5), Some("-3".into()));
        assert_eq!(snap_level(-350, -4.5), Some("-4".into()));
    }

    #[test]
    fn phone_steps_cover_the_full_console_range_and_off() {
        for db in -138i16..=10 {
            assert_eq!(
                snap_level(db * 100, f64::from(db) - 0.6),
                Some(if db == -138 {
                    "Off".into()
                } else {
                    (db - 1).to_string()
                })
            );
        }
        assert_eq!(snap_level(OFF, -138.4), Some("-138".into()));
        assert_eq!(snap_level(1000, 20.0), Some("10".into()));
        assert_eq!(snap_level(0, f64::NAN), None);
        assert_eq!(snap_level(0, f64::INFINITY), None);
    }

    #[test]
    fn assignments_are_validated_before_rendering_controls() {
        assert_eq!(mix_index("Mix 1"), Some(0));
        assert_eq!(mix_index("Mix 24"), Some(23));
        for invalid in ["Mix 0", "Mix 25", "Mix 256", "1", "Mix -1"] {
            assert_eq!(mix_index(invalid), None);
        }
    }
}
