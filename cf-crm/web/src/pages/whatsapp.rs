use dioxus::prelude::*;

use crate::api;
use crate::app::{use_session_gate, SessionSignal};

#[component]
pub fn Whatsapp() -> Element {
    let Some(current) = use_session_gate() else {
        return rsx! {
            div { class: "loading", "Carregando..." }
        };
    };
    let session = use_context::<SessionSignal>();
    let is_admin = current.role == "admin";

    let mut phone_number_id = use_signal(String::new);
    let mut display_phone = use_signal(String::new);
    let mut label = use_signal(String::new);
    let mut waba_id = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut saving = use_signal(|| false);

    let mut numbers = use_resource({
        let tenant_id = current.tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            async move { api::list_numbers(&tenant_id).await }
        }
    });

    let add = move || {
        if saving() {
            return;
        }
        let Some(s) = session() else { return };
        let (p, d, l, w) = (phone_number_id(), display_phone(), label(), waba_id());
        spawn(async move {
            saving.set(true);
            error.set(None);
            match api::add_number(&s.tenant_id, &p, &d, &l, &w).await {
                Ok(_) => {
                    phone_number_id.set(String::new());
                    display_phone.set(String::new());
                    label.set(String::new());
                    waba_id.set(String::new());
                    numbers.restart();
                }
                Err(err) => error.set(Some(err.message)),
            }
            saving.set(false);
        });
    };

    rsx! {
        div { class: "page",
            div { class: "page-header",
                h2 { "Números de WhatsApp" }
            }
            p { class: "hint",
                "No Kapso, aponte o webhook do número para "
                code { "https://<seu-domínio>/webhooks/kapso" }
                " (payload v2). Mensagem de número não cadastrado aqui é descartada."
            }
            match &*numbers.read() {
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "empty", "nenhum número cadastrado" }
                },
                Some(Ok(list)) => rsx! {
                    div { class: "table-wrap",
                        table { class: "table",
                            thead {
                                tr {
                                    th { "Número" }
                                    th { "Apelido" }
                                    th { "phone_number_id" }
                                    th { "Status" }
                                }
                            }
                            tbody {
                                for n in list.iter().cloned() {
                                    tr { key: "{n.phone_number_id}",
                                        td { "{n.display_phone}" }
                                        td { "{n.label.clone().unwrap_or_else(|| \"—\".to_string())}" }
                                        td { code { "{n.phone_number_id}" } }
                                        td { "{n.status}" }
                                    }
                                }
                            }
                        }
                    }
                },
                Some(Err(e)) => rsx! { p { class: "form-error", "{e.message}" } },
                None => rsx! { p { class: "empty", "carregando..." } },
            }
            if is_admin {
                h3 { "Cadastrar número" }
                div { class: "inline-form",
                    input {
                        r#type: "text",
                        placeholder: "phone_number_id (só dígitos, da Meta)",
                        value: "{phone_number_id}",
                        oninput: move |ev| phone_number_id.set(ev.value()),
                    }
                    input {
                        r#type: "text",
                        placeholder: "número com DDI (+55 11 90000-0000)",
                        value: "{display_phone}",
                        oninput: move |ev| display_phone.set(ev.value()),
                    }
                    input {
                        r#type: "text",
                        placeholder: "apelido (opcional)",
                        value: "{label}",
                        oninput: move |ev| label.set(ev.value()),
                    }
                    input {
                        r#type: "text",
                        placeholder: "waba_id (opcional)",
                        value: "{waba_id}",
                        oninput: move |ev| waba_id.set(ev.value()),
                        onkeydown: move |ev: KeyboardEvent| if ev.key() == Key::Enter { add() },
                    }
                    button { disabled: saving(), onclick: move |_| add(), "Cadastrar" }
                }
                if let Some(msg) = error() {
                    p { class: "form-error", "{msg}" }
                }
            } else {
                p { class: "hint", "Só administradores cadastram números." }
            }
        }
    }
}
