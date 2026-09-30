use dioxus::prelude::*;

use crate::api::{self, Board, Lead, Stage};
use crate::app::use_session_gate;

fn money(cents: Option<i64>, currency: &str) -> String {
    match cents {
        Some(c) => format!("{currency} {:.2}", c as f64 / 100.0),
        None => "—".to_string(),
    }
}

#[component]
pub fn Dashboard() -> Element {
    let Some(current) = use_session_gate() else {
        return rsx! {
            div { class: "loading", "Carregando..." }
        };
    };
    let tenant_id = current.tenant_id.clone();

    let mut selected_pipeline = use_signal(|| None::<String>);

    let pipelines = use_resource({
        let tenant_id = tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            async move { api::list_pipelines(&tenant_id).await }
        }
    });

    // Assim que os funis chegam, seleciona o primeiro (só uma vez).
    use_effect(move || {
        if let Some(Ok(list)) = &*pipelines.read() {
            if selected_pipeline.read().is_none() {
                if let Some(first) = list.first() {
                    selected_pipeline.set(Some(first.pipeline.id.clone()));
                }
            }
        }
    });

    let mut board = use_resource({
        let tenant_id = tenant_id.clone();
        move || {
            let tenant_id = tenant_id.clone();
            let pid = selected_pipeline();
            async move {
                match pid {
                    Some(id) => Some(api::get_board(&tenant_id, &id).await),
                    None => None,
                }
            }
        }
    });

    rsx! {
        div { class: "dashboard",
            match &*pipelines.read() {
                Some(Ok(list)) if list.len() > 1 => rsx! {
                    div { class: "pipeline-tabs",
                        for p in list.iter().cloned() {
                            button {
                                class: if selected_pipeline() == Some(p.pipeline.id.clone()) { "tab active" } else { "tab" },
                                onclick: {
                                    let id = p.pipeline.id.clone();
                                    move |_| selected_pipeline.set(Some(id.clone()))
                                },
                                "{p.pipeline.name}"
                            }
                        }
                    }
                },
                Some(Ok(list)) if list.is_empty() => rsx! {
                    p { class: "empty", "nenhum funil ainda" }
                },
                Some(Err(e)) => rsx! { p { class: "form-error", "{e.message}" } },
                _ => rsx! {},
            }
            match &*board.read() {
                Some(Some(Ok(b))) => rsx! {
                    BoardView {
                        board: b.clone(),
                        tenant_id: tenant_id.clone(),
                        on_changed: move |_| board.restart(),
                    }
                },
                Some(Some(Err(e))) => rsx! { p { class: "form-error", "{e.message}" } },
                _ => rsx! { p { class: "empty", "carregando..." } },
            }
        }
    }
}

#[component]
fn BoardView(board: Board, tenant_id: String, on_changed: EventHandler<()>) -> Element {
    let open_value: i64 = board
        .columns
        .iter()
        .filter(|c| !c.stage.is_won && !c.stage.is_lost)
        .flat_map(|c| c.leads.iter())
        .filter_map(|l| l.value_cents)
        .sum();
    let open_count: usize = board.columns.iter().filter(|c| !c.stage.is_won && !c.stage.is_lost).map(|c| c.leads.len()).sum();
    let won_value: i64 = board.columns.iter().filter(|c| c.stage.is_won).flat_map(|c| c.leads.iter()).filter_map(|l| l.value_cents).sum();
    let won_count: usize = board.columns.iter().filter(|c| c.stage.is_won).map(|c| c.leads.len()).sum();
    let lost_count: usize = board.columns.iter().filter(|c| c.stage.is_lost).map(|c| c.leads.len()).sum();
    let currency = board
        .columns
        .iter()
        .flat_map(|c| c.leads.iter())
        .next()
        .map(|l| l.currency.clone())
        .unwrap_or_else(|| "BRL".to_string());

    let stages: Vec<Stage> = board.columns.iter().map(|c| c.stage.clone()).collect();

    rsx! {
        div { class: "metrics",
            div { class: "metric",
                span { class: "metric-value", "{open_count}" }
                span { class: "metric-label", "em aberto ({money(Some(open_value), &currency)})" }
            }
            div { class: "metric metric-won",
                span { class: "metric-value", "{won_count}" }
                span { class: "metric-label", "ganhos ({money(Some(won_value), &currency)})" }
            }
            div { class: "metric metric-lost",
                span { class: "metric-value", "{lost_count}" }
                span { class: "metric-label", "perdidos" }
            }
        }
        div { class: "board",
            for (i , col) in board.columns.iter().enumerate() {
                ColumnView {
                    key: "{col.stage.id}",
                    stage: col.stage.clone(),
                    leads: col.leads.clone(),
                    tenant_id: tenant_id.clone(),
                    prev_stage: if i > 0 { Some(stages[i - 1].clone()) } else { None },
                    next_stage: stages.get(i + 1).cloned(),
                    on_changed: move |_| on_changed.call(()),
                }
            }
        }
    }
}

#[component]
fn ColumnView(
    stage: Stage,
    leads: Vec<Lead>,
    tenant_id: String,
    prev_stage: Option<Stage>,
    next_stage: Option<Stage>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut new_title = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);

    let add = {
        let tenant_id = tenant_id.clone();
        let stage_id = stage.id.clone();
        move |_| {
            let title = new_title();
            if title.trim().is_empty() {
                return;
            }
            let tenant_id = tenant_id.clone();
            let stage_id = stage_id.clone();
            spawn(async move {
                match api::create_lead(&tenant_id, &title, &stage_id).await {
                    Ok(_) => {
                        new_title.set(String::new());
                        on_changed.call(());
                    }
                    Err(e) => error.set(Some(e.message)),
                }
            });
        }
    };

    rsx! {
        div { class: "column",
            div { class: "column-header",
                span { class: "column-name", "{stage.name}" }
                span { class: "column-count", "{leads.len()}" }
            }
            div { class: "column-cards",
                for lead in leads.iter().cloned() {
                    {
                        let lead_id = lead.id.clone();
                        rsx! {
                            LeadCard {
                                key: "{lead_id}",
                                lead,
                                tenant_id: tenant_id.clone(),
                                prev_stage: prev_stage.clone(),
                                next_stage: next_stage.clone(),
                                on_changed: move |_| on_changed.call(()),
                            }
                        }
                    }
                }
            }
            div { class: "column-add",
                input {
                    r#type: "text",
                    placeholder: "+ novo lead",
                    value: "{new_title}",
                    oninput: move |ev| new_title.set(ev.value()),
                    onkeydown: move |ev: KeyboardEvent| if ev.key() == Key::Enter { add(()) },
                }
            }
            if let Some(msg) = error() {
                p { class: "form-error", "{msg}" }
            }
        }
    }
}

#[component]
fn LeadCard(
    lead: Lead,
    tenant_id: String,
    prev_stage: Option<Stage>,
    next_stage: Option<Stage>,
    on_changed: EventHandler<()>,
) -> Element {
    let mut error = use_signal(|| None::<String>);

    let move_to = move |target: Option<Stage>| {
        let Some(target) = target else { return };
        let lost_reason = if target.is_lost {
            match gloo_dialogs::prompt("Motivo da perda:", None) {
                Some(r) if !r.trim().is_empty() => Some(r),
                _ => return,
            }
        } else {
            None
        };
        let tenant_id = tenant_id.clone();
        let lead_id = lead.id.clone();
        spawn(async move {
            match api::move_lead(&tenant_id, &lead_id, &target.id, lost_reason.as_deref()).await {
                Ok(_) => on_changed.call(()),
                Err(e) => error.set(Some(e.message)),
            }
        });
    };

    rsx! {
        div { class: "lead-card",
            div { class: "lead-title", "{lead.title}" }
            div { class: "lead-value", "{money(lead.value_cents, &lead.currency)}" }
            if let Some(msg) = error() {
                p { class: "bubble-error", "{msg}" }
            }
            div { class: "lead-actions",
                button {
                    disabled: prev_stage.is_none(),
                    onclick: {
                        let move_to = move_to.clone();
                        move |_| move_to(prev_stage.clone())
                    },
                    "◀"
                }
                button {
                    disabled: next_stage.is_none(),
                    onclick: move |_| move_to(next_stage.clone()),
                    "▶"
                }
            }
        }
    }
}
