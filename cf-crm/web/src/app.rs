use dioxus::prelude::*;

use crate::api;
use crate::pages::{contacts::Contacts, dashboard::Dashboard, inbox::Inbox, login::Login, whatsapp::Whatsapp};

#[derive(Clone, PartialEq)]
pub struct Session {
    pub user: api::User,
    pub tenant_id: String,
    pub role: String,
}

pub type SessionSignal = Signal<Option<Session>>;

#[derive(Routable, Clone, Debug, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[route("/")]
        Inbox {},
        #[route("/dashboard")]
        Dashboard {},
        #[route("/contacts")]
        Contacts {},
        #[route("/whatsapp")]
        Whatsapp {},
    #[end_layout]
    #[route("/login")]
    Login {},
}

const MAIN_CSS: Asset = asset!("/assets/main.css");

#[component]
pub fn App() -> Element {
    use_context_provider(|| Signal::new(Option::<Session>::None));

    rsx! {
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        Router::<Route> {}
    }
}

/// Reconstrói a sessão a partir do cookie a cada entrada em `/` ou `/dashboard`
/// (recarregar a página perde o `Signal` — a sessão de verdade vive só no
/// cookie `HttpOnly`) e manda pra `/login` quando não há sessão nenhuma.
pub fn use_session_gate() -> Option<Session> {
    let mut session = use_context::<SessionSignal>();
    let nav = use_navigator();

    use_effect(move || {
        if session.read().is_none() {
            spawn(async move {
                match api::me().await {
                    Ok(me) => match me.tenants.first() {
                        Some(m) => session.set(Some(Session { user: me.user, tenant_id: m.tenant_id.clone(), role: m.role.clone() })),
                        None => {
                            nav.push(Route::Login {});
                        }
                    },
                    Err(_) => {
                        nav.push(Route::Login {});
                    }
                }
            });
        }
    });

    session()
}

#[component]
fn Shell() -> Element {
    let mut session = use_context::<SessionSignal>();
    let nav = use_navigator();
    let route = use_route::<Route>();

    let do_logout = move |_| {
        spawn(async move {
            let _ = api::logout().await;
            session.set(None);
            nav.push(Route::Login {});
        });
    };

    rsx! {
        div { class: "shell",
            header { class: "shell-header",
                nav { class: "shell-nav",
                    Link {
                        to: Route::Inbox {},
                        class: if route == (Route::Inbox {}) { "shell-link active" } else { "shell-link" },
                        "Caixa de entrada"
                    }
                    Link {
                        to: Route::Dashboard {},
                        class: if route == (Route::Dashboard {}) { "shell-link active" } else { "shell-link" },
                        "Kanban"
                    }
                    Link {
                        to: Route::Contacts {},
                        class: if route == (Route::Contacts {}) { "shell-link active" } else { "shell-link" },
                        "Contatos"
                    }
                    Link {
                        to: Route::Whatsapp {},
                        class: if route == (Route::Whatsapp {}) { "shell-link active" } else { "shell-link" },
                        "WhatsApp"
                    }
                }
                span { class: "shell-user",
                    if let Some(s) = session() {
                        "{s.user.name.clone().unwrap_or(s.user.email.clone())}"
                    }
                    button { class: "link-button", onclick: do_logout, "sair" }
                }
            }
            Outlet::<Route> {}
        }
    }
}
