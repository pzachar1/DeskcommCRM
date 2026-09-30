use dioxus::prelude::*;

use crate::api;
use crate::api::Contact;
use crate::app::{use_session_gate, PendingConversation, Route, SessionSignal};

#[component]
pub fn Contacts() -> Element {
    let Some(current) = use_session_gate() else {
        return rsx! {
            div { class: "loading", "Carregando..." }
        };
    };
    let session = use_context::<SessionSignal>();
    let mut pending = use_context::<PendingConversation>();
    let nav = use_navigator();

    let mut query = use_signal(String::new);
    let mut name = use_signal(String::new);
    let mut email = use_signal(String::new);
    let mut phone = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut saving = use_signal(|| false);

    let mut contacts = use_resource({
        let tenant_id = current.tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            let q = query();
            async move { api::list_contacts(&tenant_id, &q).await }
        }
    });

    let numbers = use_resource({
        let tenant_id = current.tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            async move { api::list_numbers(&tenant_id).await }
        }
    });
    let mut chosen_number = use_signal(|| None::<String>);

    // Número pelo qual a conversa sai: o escolhido, senão o primeiro cadastrado.
    let sender = move || -> Option<String> {
        if let Some(c) = chosen_number() {
            return Some(c);
        }
        match &*numbers.read() {
            Some(Ok(list)) => list.first().map(|n| n.phone_number_id.clone()),
            _ => None,
        }
    };

    let chat = move |contact_id: String| {
        let Some(s) = session() else { return };
        let Some(pnid) = sender() else { return };
        spawn(async move {
            error.set(None);
            match api::open_conversation(&s.tenant_id, &contact_id, &pnid).await {
                Ok(conv) => {
                    pending.set(Some(conv.id));
                    nav.push(Route::Inbox {});
                }
                Err(err) => error.set(Some(err.message)),
            }
        });
    };

    let add = move || {
        if saving() {
            return;
        }
        let Some(s) = session() else { return };
        let (n, e, p) = (name(), email(), phone());
        spawn(async move {
            saving.set(true);
            error.set(None);
            match api::create_contact(&s.tenant_id, &n, &e, &p).await {
                Ok(_) => {
                    name.set(String::new());
                    email.set(String::new());
                    phone.set(String::new());
                    contacts.restart();
                }
                Err(err) => error.set(Some(err.message)),
            }
            saving.set(false);
        });
    };

    rsx! {
        div { class: "page",
            div { class: "page-header",
                h2 { "Contatos" }
                input {
                    class: "search",
                    r#type: "text",
                    placeholder: "buscar por nome, e-mail ou telefone",
                    value: "{query}",
                    oninput: move |ev| query.set(ev.value()),
                }
            }
            div { class: "inline-form",
                input {
                    r#type: "text",
                    placeholder: "nome",
                    value: "{name}",
                    oninput: move |ev| name.set(ev.value()),
                    onkeydown: move |ev: KeyboardEvent| if ev.key() == Key::Enter { add() },
                }
                input {
                    r#type: "text",
                    placeholder: "e-mail",
                    value: "{email}",
                    oninput: move |ev| email.set(ev.value()),
                    onkeydown: move |ev: KeyboardEvent| if ev.key() == Key::Enter { add() },
                }
                input {
                    r#type: "text",
                    placeholder: "telefone com DDI (+55 11 98765-4321)",
                    value: "{phone}",
                    oninput: move |ev| phone.set(ev.value()),
                    onkeydown: move |ev: KeyboardEvent| if ev.key() == Key::Enter { add() },
                }
                button { disabled: saving(), onclick: move |_| add(), "Adicionar" }
            }
            if let Some(msg) = error() {
                p { class: "form-error", "{msg}" }
            }
            match &*numbers.read() {
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "hint", "Cadastre um número na aba WhatsApp para conversar com os contatos." }
                },
                Some(Ok(list)) if list.len() > 1 => rsx! {
                    p { class: "hint",
                        "Conversar pelo número: "
                        select {
                            onchange: move |ev| chosen_number.set(Some(ev.value())),
                            for n in list.iter().cloned() {
                                option { key: "{n.phone_number_id}", value: "{n.phone_number_id}",
                                    "{n.label.clone().unwrap_or(n.display_phone.clone())}"
                                }
                            }
                        }
                    }
                },
                _ => rsx! {},
            }
            match &*contacts.read() {
                Some(Ok(page)) if page.items.is_empty() => rsx! {
                    p { class: "empty", "nenhum contato" }
                },
                Some(Ok(page)) => rsx! {
                    div { class: "table-wrap",
                        table { class: "table",
                            thead {
                                tr {
                                    th { "Nome" }
                                    th { "Telefone" }
                                    th { "E-mail" }
                                    th {}
                                }
                            }
                            tbody {
                                for c in page.items.iter().cloned() {
                                    {
                                        let cid = c.id.clone();
                                        rsx! {
                                            ContactRow {
                                                key: "{cid}",
                                                contact: c,
                                                can_chat: sender().is_some(),
                                                on_chat: move |id| chat(id),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                Some(Err(e)) => rsx! { p { class: "form-error", "{e.message}" } },
                None => rsx! { p { class: "empty", "carregando..." } },
            }
        }
    }
}

#[component]
fn ContactRow(contact: Contact, can_chat: bool, on_chat: EventHandler<String>) -> Element {
    let dash = || "—".to_string();
    let has_phone = contact.phone_e164.is_some();
    let id = contact.id.clone();
    rsx! {
        tr {
            td {
                "{contact.name.clone().unwrap_or_else(dash)}"
                if contact.is_blocked {
                    span { class: "tag tag-blocked", "bloqueado" }
                }
            }
            td { "{contact.phone_e164.clone().unwrap_or_else(dash)}" }
            td { "{contact.email.clone().unwrap_or_else(dash)}" }
            td { class: "cell-actions",
                button {
                    class: "row-button",
                    disabled: !can_chat || !has_phone || contact.is_blocked,
                    onclick: move |_| on_chat.call(id.clone()),
                    "Conversar"
                }
            }
        }
    }
}
