use dioxus::prelude::*;

use crate::api;
use crate::pages::{inbox::Inbox, login::Login};

#[derive(Clone, PartialEq)]
pub struct Session {
    pub user: api::User,
    pub tenant_id: String,
}

pub type SessionSignal = Signal<Option<Session>>;

#[derive(Routable, Clone, Debug, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[route("/login")]
    Login {},
    #[route("/")]
    Inbox {},
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
