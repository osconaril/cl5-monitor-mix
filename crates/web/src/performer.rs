//! Performer controls use console readback as the displayed source of truth.
use super::layout::{Layout, LayoutEditor};
use super::{refresh_performer_mixes, LiveState, PerformerMixes, User};
use gloo_net::http::Request;
use gloo_timers::callback::Interval;
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

const LEVEL: &str = "MIXER:Current/InCh/ToMix/Level";
const PAN: &str = "MIXER:Current/InCh/ToMix/Pan";
const ON: &str = "MIXER:Current/InCh/ToMix/On";
const OFF: i16 = -32768;

// Only known console names become CSS colors. Never interpolate wire text into style.
fn channel_color(wire: Option<&str>) -> (&'static str, &'static str) {
    match wire.map(|v| v.trim_matches('"')) {
        Some("Blue") => ("#4285f4", "Blue"),
        Some("Orange") => ("#ff9800", "Orange"),
        Some("Yellow") => ("#ffeb3b", "Yellow"),
        Some("Purple") => ("#9c6ade", "Purple"),
        Some("Cyan") => ("#00d4df", "Cyan"),
        Some("Magenta") => ("#ed55d8", "Magenta"),
        Some("Red") => ("#f44336", "Red"),
        Some("Green") => ("#4caf50", "Green"),
        Some("Off") => ("transparent", "Off"),
        _ => ("transparent", "Unknown"),
    }
}

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
    let reordering = use_state(|| false);
    let reorder_pending = use_mut_ref(|| false);
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
    {
        let layout = layout.clone();
        let error = layout_error.clone();
        let reordering = reordering.clone();
        let pending = reorder_pending.clone();
        use_effect_with(
            (layout.is_some(), shown_layout.clone()),
            move |(_, saved)| {
                let saved = saved.clone();
                let handler = Closure::<dyn FnMut(web_sys::CustomEvent)>::new(
                    move |event: web_sys::CustomEvent| {
                        if *pending.borrow() || layout.is_none() {
                            return;
                        }
                        let Some(detail) = event.detail().as_string() else {
                            return;
                        };
                        let Ok((source, target, after)) =
                            serde_json::from_str::<(u8, u8, bool)>(&detail)
                        else {
                            return;
                        };
                        let mut next = saved.clone();
                        next.move_before(source, target);
                        if after {
                            next.move_before(target, source);
                        }
                        if next == saved {
                            return;
                        }
                        *pending.borrow_mut() = true;
                        reordering.set(true);
                        let (layout, error, reordering, pending) = (
                            layout.clone(),
                            error.clone(),
                            reordering.clone(),
                            pending.clone(),
                        );
                        spawn_local(async move {
                            let result = async {
                                let request =
                                    Request::put("/api/performer/layout").json(&next).map_err(
                                        |_| "Could not prepare your channel order.".to_owned(),
                                    )?;
                                let response = request.send().await.map_err(|_| {
                                    "Could not save your channel order. Check your connection."
                                        .to_owned()
                                })?;
                                if !response.ok() {
                                    return Err(response.text().await.unwrap_or_else(|_| {
                                        "Channel reorder was refused.".into()
                                    }));
                                }
                                response.json::<Layout>().await.map_err(|_| {
                                    "Could not read the saved channel order.".to_owned()
                                })
                            }
                            .await;
                            match result {
                                Ok(saved) => {
                                    layout.set(Some(saved));
                                    error.set(String::new());
                                }
                                Err(message) => error.set(message),
                            }
                            *pending.borrow_mut() = false;
                            reordering.set(false);
                        });
                    },
                );
                let window = web_sys::window().expect("browser window");
                window
                    .add_event_listener_with_callback(
                        "cl5-reorder",
                        handler.as_ref().unchecked_ref(),
                    )
                    .expect("reorder listener");
                move || {
                    let _ = window.remove_event_listener_with_callback(
                        "cl5-reorder",
                        handler.as_ref().unchecked_ref(),
                    );
                    drop(handler);
                }
            },
        );
    }
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
    {
        let selected = selected.clone();
        use_effect_with(assignments.clone(), move |assignments| {
            if selected.is_none() && assignments.len() == 1 {
                selected.set(assignments.first().cloned());
            }
            || ()
        });
    }
    // A revoked assignment immediately removes its controls. Never silently
    // retarget an old strip to a different mix.
    let active = if let Some(name) = selected.as_ref() {
        assignments.contains(name).then(|| name.clone())
    } else if assignments.len() == 1 {
        assignments.first().cloned()
    } else {
        None
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
    let solo = {
        let message = message.clone();
        let live = live.clone();
        Callback::from(move |(mix, input): (u8, u8)| {
            let mut channels = live
                .console
                .solos
                .get(&mix.to_string())
                .map(|s| s.channels.clone())
                .unwrap_or_default();
            if !channels.remove(&input) {
                channels.insert(input);
            }
            let message = message.clone();
            spawn_local(async move {
                let request = Request::post("/api/performer/solo")
                    .json(&serde_json::json!({"mix": mix, "channels": channels}));
                let response = match request {
                    Ok(request) => request.send().await,
                    Err(_) => {
                        message.set("Could not prepare solo. Try again.".into());
                        return;
                    }
                };
                match response {
                    Ok(response) if response.ok() => message.set(String::new()),
                    Ok(response) => message.set(
                        response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Solo was refused.".into()),
                    ),
                    Err(_) => message.set(
                        "The show machine could not be reached. Check your connection.".into(),
                    ),
                }
            });
        })
    };
    let back = {
        let selected = selected.clone();
        Callback::from(move |_| selected.set(None))
    };
    html! {
        <section class="performer">
            <h1>{format!("{}’s in-ear mix", props.user.name)}</h1>
            <p role="alert">{(*message).clone()}</p>
            if selected.as_ref().is_some_and(|name| !assignments.contains(name)) {
                <p role="alert">{"That mix is no longer assigned to you. Choose an assigned mix or ask the engineer."}</p>
            }
            if let Some(mix) = active.as_deref().and_then(mix_index) {
                <div class="mix-heading"><h2>{active.clone().unwrap_or_default()}</h2>
                if assignments.len() > 1 { <button onclick={back}>{"Choose another mix"}</button> }</div>
                if !live.connected { <p role="alert">{"Console disconnected — controls are unavailable until it reconnects."}</p> }
                if let Some(status) = live.console.solos.get(&mix.to_string()) {
                    <p role="status">{format!("Solo active (by {})", status.owner)}</p>
                }
                if let Some(notice) = &live.console.solo_notice { <p role="status">{notice}</p> }
                <p role="alert">{(*layout_error).clone()}</p>
                if !layout_error.is_empty() { <button onclick={retry_layout} disabled={*reordering}>{"Retry loading layout"}</button> }
                if *editing {
                    <LayoutEditor layout={shown_layout.clone()} names={channel_names.clone()} saved={saved_layout} cancel={cancel_layout} />
                } else {
                <button onclick={edit_layout} disabled={layout.is_none() || *reordering}>{"Arrange"}</button>
                if *reordering { <p role="status">{"Saving channel order…"}</p> }
                <p class="fader-hint">{"Touch and hold a fader to adjust. Swipe to scroll."}</p>
                <div class="channel-strips" tabindex="0" aria-label="Mix channels">
                {for shown_layout.groups.iter().map(|group| html! {
                    <GroupStrip key={format!("{mix}-{}", group.id)} mix={mix} group={group.clone()} connected={live.connected}
                        levels={group.channels.iter().map(|input| live.console.values.get(&format!("{LEVEL} {input} {mix}")).and_then(|v| v.parse::<i16>().ok())).collect::<Vec<_>>()} />
                })}
                {for shown_layout.order.iter().copied().filter(|input| !shown_layout.hidden.contains(input)).map(|input| {
                    let value = |path| live.console.values.get(&format!("{path} {input} {mix}"));
                    let level = value(LEVEL).and_then(|v| v.parse::<i16>().ok()).filter(|v| *v == OFF || (-13800..=1000).contains(v));
                    let stereo = live.console.values.get(&format!("MIXER:Current/Mix/Role {mix} 0")).is_some_and(|r| matches!(r.trim_matches('"'), "StereoL" | "StereoR"));
                    let pan = value(PAN).and_then(|v| v.parse::<i16>().ok()).filter(|v| (-63..=63).contains(v));
                    let on = value(ON).and_then(|v| match v.as_str() { "1" => Some(true), "0" => Some(false), _ => None });
                    let name = channel_names[input as usize].clone();
                    let color = live.console.values.get(&format!("MIXER:Current/InCh/Label/Color {input} 0")).cloned();
                    html! { <ChannelStrip key={format!("{mix}-{input}")} {mix} {input} {name} {color} {level} {on} {stereo} {pan} connected={live.connected} reorder_allowed={layout.is_some() && !*reordering} change={change.clone()} solo={solo.clone()}
                        soloed={live.console.solos.get(&mix.to_string()).is_some_and(|s| s.channels.contains(&input))}
                        solo_allowed={live.console.solos.get(&mix.to_string()).is_none_or(|s| s.owner == props.user.name)} /> }
                })}
                </div>
                if !shown_layout.hidden.is_empty() {
                    <section class="hidden-channels"><h2>{"Hidden"}</h2>
                    <p>{shown_layout.hidden.iter().map(|input| channel_names[*input as usize].clone()).collect::<Vec<_>>().join(", ")}</p>
                    <p>{"Open Arrange to show these channels again."}</p></section>
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
    color: Option<String>,
    level: Option<i16>,
    on: Option<bool>,
    stereo: bool,
    pan: Option<i16>,
    connected: bool,
    reorder_allowed: bool,
    change: Callback<(u8, u8, &'static str, String)>,
    solo: Callback<(u8, u8)>,
    soloed: bool,
    solo_allowed: bool,
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
    let pan_input = {
        let change = props.change.clone();
        let (mix, input) = (props.mix, props.input);
        Callback::from(move |event: InputEvent| {
            let target: web_sys::HtmlInputElement = event.target_unchecked_into();
            let value = target.value_as_number().round().clamp(-63.0, 63.0) as i16;
            target.set_value(&value.to_string());
            change.emit((mix, input, PAN, value.to_string()));
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
    let solo = {
        let callback = props.solo.clone();
        let (mix, input) = (props.mix, props.input);
        Callback::from(move |_| callback.emit((mix, input)))
    };
    let (color, color_name) = channel_color(props.color.as_deref());
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
            <div class="strip-heading">
            <span class="channel-color" role="img" aria-label={format!("Channel color: {color_name}")} title={format!("Channel color: {color_name}")} style={format!("background-color: {color}")}></span>
            <h3>{&props.name}</h3>
            <button class="reorder-handle" aria-label={format!("Drag {} to reorder", props.name)} disabled={!props.reorder_allowed}>{"↕"}</button></div>
            <label class="send-control">
                <span>{props.level.map(level_text).unwrap_or_else(|| "Level unavailable".into())}</span>
                <input type="range" aria-label={format!("{} send level", props.name)} aria-valuetext={props.level.map(level_text)}
                    min="-139" max="10" step="any" value={position.to_string()} {oninput} disabled={!props.connected || props.level.is_none()} />
            </label>
            if props.stereo {
                <label class="send-control pan-control">
                    <span>{props.pan.map(|v| if v == 0 { "Pan: center".into() } else { format!("Pan: {} {}", if v < 0 { "L" } else { "R" }, v.abs()) }).unwrap_or_else(|| "Pan unavailable".into())}</span>
                    <input type="range" aria-label={format!("{} pan", props.name)} min="-63" max="63" step="1"
                        value={props.pan.unwrap_or(0).to_string()} oninput={pan_input} disabled={!props.connected || props.pan.is_none()} />
                </label>
            }
            <button data-action="mute" onclick={mute} disabled={!props.connected || props.on.is_none()} aria-pressed={props.on.map(|on| (!on).to_string())}>
                {if props.on == Some(false) { "Muted — unmute" } else { "Mute" }}
            </button>
            <button data-action="solo" onclick={solo} disabled={!props.connected || !props.solo_allowed || props.level.is_none()} aria-pressed={props.soloed.to_string()}>
                {"Solo"}
            </button>
        </article>
    }
}

#[derive(Properties, PartialEq)]
struct GroupStripProps {
    mix: u8,
    group: super::layout::Group,
    connected: bool,
    levels: Vec<Option<i16>>,
}

#[function_component(GroupStrip)]
fn group_strip(props: &GroupStripProps) -> Html {
    let draft = use_state(|| 0i16);
    let pending = use_state(|| false);
    let sending = use_mut_ref(|| false);
    let message = use_state(String::new);
    let node = use_node_ref();
    {
        let draft = draft.clone();
        let node = node.clone();
        use_effect_with((), move |_| {
            let input = node
                .cast::<web_sys::HtmlInputElement>()
                .expect("group range");
            let reset = Closure::<dyn Fn(web_sys::Event)>::new(move |_| draft.set(0));
            input
                .add_event_listener_with_callback("cl5-cancel", reset.as_ref().unchecked_ref())
                .expect("cancel listener");
            move || {
                let _ = input.remove_event_listener_with_callback(
                    "cl5-cancel",
                    reset.as_ref().unchecked_ref(),
                );
            }
        });
    }
    let oninput = {
        let draft = draft.clone();
        Callback::from(move |event: InputEvent| {
            let input: web_sys::HtmlInputElement = event.target_unchecked_into();
            let value = input.value_as_number().round().clamp(-149.0, 149.0) as i16;
            input.set_value(&value.to_string());
            draft.set(value);
        })
    };
    let onchange = {
        let draft = draft.clone();
        let pending = pending.clone();
        let sending = sending.clone();
        let message = message.clone();
        let mix = props.mix;
        let group = props.group.id.clone();
        Callback::from(move |event: Event| {
            let input: web_sys::HtmlInputElement = event.target_unchecked_into();
            let delta = input.value_as_number().round() as i16;
            input.set_value("0");
            draft.set(0);
            if delta == 0 || *sending.borrow() {
                return;
            }
            *sending.borrow_mut() = true;
            pending.set(true);
            let (pending, sending, message, group) = (
                pending.clone(),
                sending.clone(),
                message.clone(),
                group.clone(),
            );
            spawn_local(async move {
                let result = async {
                    let response = Request::post("/api/performer/group")
                        .json(&serde_json::json!({"mix":mix,"group":group,"delta":delta}))
                        .map_err(|_| "Could not prepare group movement.".to_owned())?
                        .send()
                        .await
                        .map_err(|_| {
                            "The show machine could not be reached. Check your connection."
                                .to_owned()
                        })?;
                    if response.ok() {
                        Ok(())
                    } else {
                        Err(response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Group movement was refused.".into()))
                    }
                }
                .await;
                message.set(result.err().unwrap_or_default());
                *sending.borrow_mut() = false;
                pending.set(false);
            });
        })
    };
    let available = props
        .levels
        .iter()
        .all(|v| v.is_some_and(|v| v == OFF || (-13800..=1000).contains(&v)));
    html! {
        <article class="channel-strip group-strip" data-channel={format!("group-{}", props.group.id)}>
            <h3>{&props.group.name}</h3>
            <label class="send-control">
                <span>{format!("{:+} dB on release", *draft)}</span>
                <input ref={node} data-group-fader="true" type="range" min="-149" max="149" step="1" value={draft.to_string()}
                    aria-label={format!("{} group level change", props.group.name)} {oninput} {onchange}
                    disabled={!props.connected || !available || *pending} />
            </label>
            <small>{"Whole-dB change; Off stays Off."}</small>
            <p role="alert">{(*message).clone()}</p>
        </article>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_readback_uses_only_known_names() {
        for name in [
            "Blue", "Orange", "Yellow", "Purple", "Cyan", "Magenta", "Red", "Green", "Off",
        ] {
            assert_eq!(channel_color(Some(&format!("\"{name}\""))).1, name);
        }
        for value in [
            None,
            Some("0"),
            Some("-32768"),
            Some("url(https://example.com)"),
            Some("Unknown"),
        ] {
            assert_eq!(channel_color(value), ("transparent", "Unknown"));
        }
    }

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
