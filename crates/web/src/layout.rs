//! Layout edits are staged locally and become visible only after server readback.
use gloo_net::http::Request;
use serde::{Deserialize, Serialize};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Layout {
    pub order: Vec<u8>,
    pub hidden: Vec<u8>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Group {
    pub id: String,
    pub name: String,
    pub channels: Vec<u8>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            order: (0..80).collect(),
            hidden: vec![],
            groups: vec![],
        }
    }
}

impl Layout {
    pub(super) fn move_before(&mut self, source: u8, target: u8) {
        if source == target || !self.order.contains(&source) || !self.order.contains(&target) {
            return;
        }
        self.order.retain(|channel| *channel != source);
        let position = self
            .order
            .iter()
            .position(|channel| *channel == target)
            .unwrap();
        self.order.insert(position, source);
    }
}

#[derive(Properties, PartialEq)]
#[allow(dead_code)]
pub(super) struct EditorProps {
    pub layout: Layout,
    pub names: Vec<String>,
    pub saved: Callback<Layout>,
    pub cancel: Callback<MouseEvent>,
}

#[function_component(LayoutEditor)]
pub(super) fn layout_editor(props: &EditorProps) -> Html {
    let draft = use_state(|| props.layout.clone());
    let saving = use_state(|| false);
    let in_flight = use_mut_ref(|| false);
    let error = use_state(String::new);
    let dragged = use_mut_ref(|| None::<u8>);
    let group_name = use_state(String::new);
    let members = use_state(Vec::<u8>::new);
    let save = {
        let draft = draft.clone();
        let saving = saving.clone();
        let in_flight = in_flight.clone();
        let error = error.clone();
        let saved = props.saved.clone();
        Callback::from(move |_| {
            if *in_flight.borrow() {
                return;
            }
            *in_flight.borrow_mut() = true;
            saving.set(true);
            let (saving, in_flight, error, saved) = (
                saving.clone(),
                in_flight.clone(),
                error.clone(),
                saved.clone(),
            );
            let body = (*draft).clone();
            spawn_local(async move {
                let result = async {
                    let request = Request::put("/api/performer/layout")
                        .json(&body)
                        .map_err(|_| "Could not prepare your layout.".to_owned())?;
                    let response = request.send().await.map_err(|_| {
                        "Could not reach the show machine. Your layout is not saved. Try again."
                            .to_owned()
                    })?;
                    if !response.ok() {
                        return Err(response
                            .text()
                            .await
                            .unwrap_or_else(|_| "Your layout was refused.".into()));
                    }
                    response.json::<Layout>().await.map_err(|_| {
                        "Could not read back your saved layout. Reload before editing again."
                            .to_owned()
                    })
                }
                .await;
                *in_flight.borrow_mut() = false;
                saving.set(false);
                match result {
                    Ok(layout) => saved.emit(layout),
                    Err(message) => error.set(message),
                }
            });
        })
    };
    let name_input = {
        let group_name = group_name.clone();
        Callback::from(move |event: InputEvent| {
            let input: web_sys::HtmlInputElement = event.target_unchecked_into();
            group_name.set(input.value());
        })
    };
    let add_group = {
        let draft = draft.clone();
        let group_name = group_name.clone();
        let members = members.clone();
        Callback::from(move |_| {
            if group_name.trim().is_empty() || members.is_empty() {
                return;
            }
            let mut next = (*draft).clone();
            let id = (0..)
                .map(|n| format!("group-{n}"))
                .find(|id| !next.groups.iter().any(|group| group.id == *id))
                .unwrap();
            next.groups.push(Group {
                id,
                name: group_name.trim().into(),
                channels: (*members).clone(),
            });
            draft.set(next);
            group_name.set(String::new());
            members.set(vec![]);
        })
    };
    html! {
        <section class="layout-editor" aria-label="Edit your layout">
            <h2>{"Your channel layout"}</h2>
            <p>{"Drag a channel onto another to place it before that channel, or use Earlier and Later. Layout changes do not change the sound."}</p>
            <p role="alert">{(*error).clone()}</p>
            <fieldset disabled={*saving}>
                <legend>{"Order and visibility"}</legend>
                {for draft.order.iter().enumerate().map(|(position, &input)| {
                    let hide = {
                        let draft = draft.clone();
                        Callback::from(move |_| {
                            let mut next = (*draft).clone();
                            if next.hidden.contains(&input) { next.hidden.retain(|n| *n != input); }
                            else { next.hidden.push(input); }
                            draft.set(next);
                        })
                    };
                    let shift = |offset: isize| {
                        let draft = draft.clone();
                        Callback::from(move |_| {
                            let mut next = (*draft).clone();
                            let destination = position as isize + offset;
                            if (0..next.order.len() as isize).contains(&destination) {
                                next.order.swap(position, destination as usize);
                                draft.set(next);
                            }
                        })
                    };
                    let ondragstart = {
                        let dragged = dragged.clone();
                        Callback::from(move |_: DragEvent| *dragged.borrow_mut() = Some(input))
                    };
                    let ondragend = {
                        let dragged = dragged.clone();
                        Callback::from(move |_: DragEvent| *dragged.borrow_mut() = None)
                    };
                    let ondrop = {
                        let dragged = dragged.clone();
                        let draft = draft.clone();
                        let in_flight = in_flight.clone();
                        Callback::from(move |event: DragEvent| {
                            event.prevent_default();
                            if *in_flight.borrow() { return; }
                            if let Some(source) = dragged.borrow_mut().take() {
                                let mut next = (*draft).clone();
                                next.move_before(source, input);
                                draft.set(next);
                            }
                        })
                    };
                    html! {
                        <div class="layout-channel" key={input} data-layout-channel={input.to_string()}
                            draggable={(!*saving).to_string()} {ondragstart} {ondragend} {ondrop}
                            ondragover={Callback::from(|event: DragEvent| event.prevent_default())}>
                            <strong>{format!("{} · {}", input + 1, props.names[input as usize])}</strong>
                            <button onclick={shift(-1)} disabled={position == 0}>{"Earlier"}</button>
                            <button onclick={shift(1)} disabled={position == 79}>{"Later"}</button>
                            <button onclick={hide}>{if draft.hidden.contains(&input) { "Show channel" } else { "Hide channel" }}</button>
                        </div>
                    }
                })}
                <h3>{"Named groups"}</h3>
                {for draft.groups.iter().map(|group| {
                    let rename = {
                        let draft = draft.clone();
                        let id = group.id.clone();
                        Callback::from(move |event: InputEvent| {
                            let input: web_sys::HtmlInputElement = event.target_unchecked_into();
                            let mut next = (*draft).clone();
                            if let Some(group) = next.groups.iter_mut().find(|g| g.id == id) { group.name = input.value(); }
                            draft.set(next);
                        })
                    };
                    let delete = {
                        let draft = draft.clone();
                        let id = group.id.clone();
                        Callback::from(move |_| {
                            let mut next = (*draft).clone();
                            next.groups.retain(|g| g.id != id);
                            draft.set(next);
                        })
                    };
                    html! { <div class="layout-group" key={group.id.clone()}>
                        <label>{"Group name"}<input value={group.name.clone()} maxlength="80" oninput={rename} /></label>
                        <p>{group.channels.iter().map(|n| props.names[*n as usize].clone()).collect::<Vec<_>>().join(", ")}</p>
                        <button onclick={delete}>{"Delete group"}</button>
                    </div> }
                })}
                <label>{"New group name"}<input aria-label="New group name" value={(*group_name).clone()} maxlength="80" oninput={name_input} /></label>
                <fieldset><legend>{"Choose group channels"}</legend>
                {for draft.order.iter().map(|&input| {
                    let onchange = {
                        let members = members.clone();
                        Callback::from(move |event: Event| {
                            let checkbox: web_sys::HtmlInputElement = event.target_unchecked_into();
                            let mut next = (*members).clone();
                            next.retain(|n| *n != input);
                            if checkbox.checked() { next.push(input); }
                            members.set(next);
                        })
                    };
                    html! { <label class="group-member"><input type="checkbox" value={input.to_string()}
                        checked={members.contains(&input)} {onchange} />{format!("{} · {}", input + 1, props.names[input as usize])}</label> }
                })}
                </fieldset>
                <button onclick={add_group} disabled={group_name.trim().is_empty() || members.is_empty() || draft.groups.len() >= 80}>{"Add group"}</button>
                <button onclick={save}>{"Save layout"}</button>
                <button onclick={props.cancel.clone()}>{"Cancel layout changes"}</button>
            </fieldset>
            if *saving { <p role="status">{"Saving your layout…"}</p> }
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_channels_preserves_hidden_channels_and_group_membership() {
        let mut layout = Layout {
            hidden: vec![2],
            ..Layout::default()
        };
        layout.groups.push(Group {
            id: "band".into(),
            name: "Band".into(),
            channels: vec![0, 2],
        });
        layout.move_before(79, 0);
        assert_eq!(&layout.order[..3], &[79, 0, 1]);
        layout.move_before(0, 2);
        assert_eq!(&layout.order[..4], &[79, 1, 0, 2]);
        let before = layout.clone();
        layout.move_before(80, 1);
        layout.move_before(0, 80);
        layout.move_before(1, 1);
        assert_eq!(layout, before);
        let mut sorted = layout.order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..80).collect::<Vec<_>>());
        assert_eq!(layout.hidden, vec![2]);
        assert_eq!(layout.groups[0].channels, vec![0, 2]);
    }
}
