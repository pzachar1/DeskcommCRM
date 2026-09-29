use dioxus::prelude::*;

use crate::api;
use crate::app::{use_session_gate, SessionSignal};

#[component]
pub fn Contacts() -> Element {
    let Some(current) = use_session_gate() else {
        return rsx! {
            div { class: "loading", "Carregando..." }
        };
    };
    let session = use_context::<SessionSignal>();

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
            match &*contacts.read() {
                Some(Ok(page)) if page.items.is_empty() => rsx! {
                    p { class: "empty", "nenhum contato" }
                },
                Some(Ok(page)) => rsx! {
                    table { class: "table",
                        thead {
                            tr {
                                th { "Nome" }
                                th { "Telefone" }
                                th { "E-mail" }
                            }
                        }
                        tbody {
                            for c in page.items.iter().cloned() {
                                tr { key: "{c.id}",
                                    td {
                                        "{c.name.clone().unwrap_or_else(|| \"—\".to_string())}"
                                        if c.is_blocked {
                                            span { class: "tag tag-blocked", "bloqueado" }
                                        }
                                    }
                                    td { "{c.phone_e164.clone().unwrap_or_else(|| \"—\".to_string())}" }
                                    td { "{c.email.clone().unwrap_or_else(|| \"—\".to_string())}" }
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
