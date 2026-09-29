use dioxus::prelude::*;

use crate::api;
use crate::app::{Route, Session, SessionSignal};

#[component]
pub fn Login() -> Element {
    let mut session = use_context::<SessionSignal>();
    let nav = use_navigator();

    let mut email = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let mut loading = use_signal(|| false);

    // Sem `<form>`: dioxus-web precisaria de um `prevent_default` declarado à
    // parte para barrar o POST nativo do form (que recarrega a página e
    // derruba o WASM). Um `div` com botão evita o problema inteiro.
    let submit = move || {
        if loading() {
            return;
        }
        let email_v = email();
        let password_v = password();
        spawn(async move {
            loading.set(true);
            error.set(None);
            if let Err(e) = api::login(&email_v, &password_v).await {
                error.set(Some(e.message));
                loading.set(false);
                return;
            }
            match api::me().await {
                Ok(me) => match me.tenants.first() {
                    Some(m) => {
                        session.set(Some(Session { user: me.user, tenant_id: m.tenant_id.clone() }));
                        nav.push(Route::Inbox {});
                    }
                    None => error.set(Some("sua conta ainda não tem nenhum tenant".into())),
                },
                Err(e) => error.set(Some(e.message)),
            }
            loading.set(false);
        });
    };

    rsx! {
        div { class: "login-page",
            div { class: "login-form",
                h1 { "DeskcommCRM" }
                if let Some(msg) = error() {
                    p { class: "form-error", "{msg}" }
                }
                label {
                    "E-mail"
                    input {
                        r#type: "email",
                        value: "{email}",
                        required: true,
                        autofocus: true,
                        oninput: move |ev| email.set(ev.value()),
                        onkeydown: {
                            let submit = submit.clone();
                            move |ev: KeyboardEvent| if ev.key() == Key::Enter { submit() }
                        },
                    }
                }
                label {
                    "Senha"
                    input {
                        r#type: "password",
                        value: "{password}",
                        required: true,
                        oninput: move |ev| password.set(ev.value()),
                        onkeydown: {
                            let submit = submit.clone();
                            move |ev: KeyboardEvent| if ev.key() == Key::Enter { submit() }
                        },
                    }
                }
                button { onclick: move |_| submit(), disabled: loading(),
                    if loading() { "Entrando..." } else { "Entrar" }
                }
            }
        }
    }
}
