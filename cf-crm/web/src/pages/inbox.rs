use dioxus::prelude::*;

use crate::api::{self, InboxItem, Message};
use crate::app::use_session_gate;

#[component]
pub fn Inbox() -> Element {
    let Some(current) = use_session_gate() else {
        return rsx! {
            div { class: "loading", "Carregando..." }
        };
    };
    let tenant_id = current.tenant_id.clone();

    let mut selected = use_signal(|| None::<String>);
    let mut compose = use_signal(String::new);
    let mut send_error = use_signal(|| None::<String>);

    let mut conversations = use_resource({
        let tenant_id = tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            async move { api::list_conversations(&tenant_id, None).await }
        }
    });

    let mut messages = use_resource({
        let tenant_id = tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            let conv_id = selected();
            async move {
                match conv_id {
                    Some(id) => Some(api::list_messages(&tenant_id, &id).await),
                    None => None,
                }
            }
        }
    });

    let open_conversation = move |item: InboxItem| {
        let tenant_id = tenant_id.clone();
        let conv_id = item.conversation.id.clone();
        selected.set(Some(conv_id.clone()));
        spawn(async move {
            let _ = api::mark_read(&tenant_id, &conv_id).await;
            conversations.restart();
        });
    };

    let send = {
        let tenant_id = current.tenant_id.clone();
        move |_| {
            let Some(conv_id) = selected() else { return };
            let body = compose();
            if body.trim().is_empty() {
                return;
            }
            let tenant_id = tenant_id.clone();
            spawn(async move {
                send_error.set(None);
                match api::send_text(&tenant_id, &conv_id, &body).await {
                    Ok(_) => {
                        compose.set(String::new());
                        messages.restart();
                        conversations.restart();
                    }
                    Err(e) => send_error.set(Some(e.message)),
                }
            });
        }
    };

    rsx! {
        div { class: "inbox",
            aside { class: "conversation-list",
                match &*conversations.read() {
                    Some(Ok(page)) if page.items.is_empty() => rsx! {
                        p { class: "empty", "nenhuma conversa ainda" }
                    },
                    Some(Ok(page)) => rsx! {
                        for item in page.items.iter().cloned() {
                            ConversationRow {
                                item: item.clone(),
                                active: selected() == Some(item.conversation.id.clone()),
                                onselect: open_conversation.clone(),
                            }
                        }
                    },
                    Some(Err(e)) => rsx! { p { class: "form-error", "{e.message}" } },
                    None => rsx! { p { class: "empty", "carregando..." } },
                }
            }
            main { class: "thread",
                if let Some(conv_id) = selected() {
                    div { class: "thread-messages",
                        match &*messages.read() {
                            Some(Some(Ok(page))) => rsx! {
                                for m in page.items.iter().cloned() {
                                    MessageBubble { message: m }
                                }
                            },
                            Some(Some(Err(e))) => rsx! { p { class: "form-error", "{e.message}" } },
                            _ => rsx! { p { class: "empty", "carregando mensagens..." } },
                        }
                    }
                    div {
                        class: "composer",
                        input {
                            r#type: "text",
                            placeholder: "escreva uma mensagem",
                            value: "{compose}",
                            oninput: move |ev| compose.set(ev.value()),
                            onkeydown: {
                                let send = send.clone();
                                move |ev: KeyboardEvent| if ev.key() == Key::Enter { send(()) }
                            },
                        }
                        button { onclick: move |_| send(()), "Enviar" }
                    }
                    if let Some(msg) = send_error() {
                        p { class: "form-error", "{msg}" }
                    }
                    { let _ = conv_id; }
                } else {
                    div { class: "empty-thread", "selecione uma conversa" }
                }
            }
        }
    }
}

#[component]
fn ConversationRow(item: InboxItem, active: bool, onselect: EventHandler<InboxItem>) -> Element {
    let name = item.contact_name.clone().or_else(|| item.contact_phone.clone()).unwrap_or_else(|| "sem nome".to_string());
    let preview = item.conversation.last_message_preview.clone().unwrap_or_default();
    let class = if active { "conversation-row active" } else { "conversation-row" };
    rsx! {
        button {
            class,
            onclick: move |_| onselect.call(item.clone()),
            div { class: "conversation-row-name", "{name}" }
            div { class: "conversation-row-preview", "{preview}" }
            if item.conversation.unread_count > 0 {
                span { class: "badge", "{item.conversation.unread_count}" }
            }
        }
    }
}

#[component]
fn MessageBubble(message: Message) -> Element {
    let class = if message.direction == "outbound" { "bubble outbound" } else { "bubble inbound" };
    rsx! {
        div { class,
            p { "{message.body.clone().unwrap_or_default()}" }
            if let Some(err) = message.error_message.clone() {
                p { class: "bubble-error", "{err}" }
            }
        }
    }
}
