mod common;

use common::{Env, SqliteDb, NOW};
use crm_core::contacts::{self, ContactPatch, NewContact};
use crm_core::leads::{self, LeadPatch, MoveLead, NewLead};
use crm_core::migrate::{ensure_tenant, migrate};
use crm_core::pipelines::{self, NewPipeline};
use crm_core::{activities, CoreError};
use crm_schema::model::LeadStatus;
use crm_schema::TENANT_MIGRATIONS;

fn funnel(env: &Env) -> pipelines::PipelineWithStages {
    let c = ctx!(env);
    pipelines::create(&c, NewPipeline { name: "Vendas".into(), slug: "vendas".into(), description: None, stages: None, vocabulary: None }).unwrap()
}

fn lead(env: &Env, title: &str) -> crm_schema::model::Lead {
    let c = ctx!(env);
    leads::create(&c, NewLead { title: title.into(), ..Default::default() }).unwrap()
}

fn code(e: CoreError) -> &'static str {
    e.code()
}

fn column(env: &Env, pipeline_id: &str, slug: &str) -> Vec<String> {
    let c = ctx!(env);
    let b = leads::board(&c, pipeline_id).unwrap();
    b.columns.into_iter().find(|col| col.stage.slug == slug).unwrap().leads.into_iter().map(|l| l.title).collect()
}

// ------------------------------------------------------------------ migrate

#[test]
fn migrate_is_idempotent_and_records_version() {
    let db = SqliteDb::new();
    let last = TENANT_MIGRATIONS.last().unwrap().version;
    assert_eq!(migrate(&db, TENANT_MIGRATIONS, NOW).unwrap(), last);
    assert_eq!(migrate(&db, TENANT_MIGRATIONS, NOW + 1).unwrap(), last, "rodar de novo não reaplica");
}

#[test]
fn tenant_guard_refuses_other_tenant() {
    let env = Env::new();
    ensure_tenant(&env.db, "tenant-a").unwrap();
    assert_eq!(code(ensure_tenant(&env.db, "tenant-b").unwrap_err()), "tenant_mismatch");
}

// ------------------------------------------------------------------ contacts

#[test]
fn contact_phone_is_normalized_and_unique() {
    let env = Env::new();
    let c = ctx!(env);
    let a = contacts::create(&c, NewContact { name: Some("Ana".into()), phone: Some("+55 (11) 98765-4321".into()), ..Default::default() }).unwrap();
    assert_eq!(a.phone_e164.as_deref(), Some("+5511987654321"));

    let dup = contacts::create(&c, NewContact { phone: Some("0055 11 98765 4321".into()), ..Default::default() }).unwrap_err();
    assert_eq!(dup.status(), 409);

    let no_ddi = contacts::create(&c, NewContact { phone: Some("11987654321".into()), ..Default::default() }).unwrap_err();
    assert_eq!(code(no_ddi), "invalid_phone");
}

#[test]
fn contact_needs_some_identity() {
    let env = Env::new();
    let c = ctx!(env);
    assert_eq!(code(contacts::create(&c, NewContact { name: Some("  ".into()), ..Default::default() }).unwrap_err()), "empty_contact");
    let a = contacts::create(&c, NewContact { name: Some("Ana".into()), ..Default::default() }).unwrap();
    let e = contacts::update(&c, &a.id, ContactPatch { name: Some(None), ..Default::default() }).unwrap_err();
    assert_eq!(code(e), "empty_contact");
}

#[test]
fn contact_patch_distinguishes_absent_from_null() {
    let env = Env::new();
    let c = ctx!(env);
    let a = contacts::create(&c, NewContact { name: Some("Ana".into()), email: Some("ana@x.com".into()), ..Default::default() }).unwrap();
    let p: ContactPatch = serde_json::from_str(r#"{"email": null}"#).unwrap();
    let a = contacts::update(&c, &a.id, p).unwrap();
    assert_eq!(a.email, None);
    assert_eq!(a.name.as_deref(), Some("Ana"), "campo ausente não mexe");
}

#[test]
fn contact_list_paginates_and_searches() {
    let env = Env::new();
    for i in 0..5 {
        let c = ctx!(env, NOW + i);
        contacts::create(&c, NewContact { name: Some(format!("Pessoa {i}")), ..Default::default() }).unwrap();
    }
    let c = ctx!(env);
    let p1 = contacts::list(&c, 2, None, None).unwrap();
    assert_eq!(p1.items.iter().map(|x| x.name.clone().unwrap()).collect::<Vec<_>>(), ["Pessoa 4", "Pessoa 3"]);
    let p2 = contacts::list(&c, 2, p1.next_cursor.as_deref(), None).unwrap();
    assert_eq!(p2.items[0].name.as_deref(), Some("Pessoa 2"));
    let p3 = contacts::list(&c, 2, p2.next_cursor.as_deref(), None).unwrap();
    assert_eq!(p3.items.len(), 1);
    assert!(p3.next_cursor.is_none());

    let found = contacts::list(&c, 10, None, Some("soa 3")).unwrap();
    assert_eq!(found.items.len(), 1);
    let pct = contacts::list(&c, 10, None, Some("%")).unwrap();
    assert!(pct.items.is_empty(), "% é literal, não curinga");
}

// ------------------------------------------------------------------ pipelines

#[test]
fn first_pipeline_is_default_with_default_stages() {
    let env = Env::new();
    let p = funnel(&env);
    assert!(p.pipeline.is_default);
    let slugs: Vec<_> = p.stages.iter().map(|s| s.slug.as_str()).collect();
    assert_eq!(slugs, ["novo", "qualificado", "proposta", "ganho", "perdido"]);
    assert!(p.stages[3].is_won && p.stages[4].is_lost);

    let c = ctx!(env);
    let second = pipelines::create(&c, NewPipeline { name: "Pós".into(), slug: "pos".into(), description: None, stages: None, vocabulary: None }).unwrap();
    assert!(!second.pipeline.is_default);
    assert_eq!(pipelines::list(&c).unwrap().len(), 2);
}

#[test]
fn pipeline_slug_is_validated_and_unique() {
    let env = Env::new();
    funnel(&env);
    let c = ctx!(env);
    let bad = pipelines::create(&c, NewPipeline { name: "X".into(), slug: "Com Espaço".into(), description: None, stages: None, vocabulary: None });
    assert_eq!(code(bad.unwrap_err()), "invalid_slug");
    let dup = pipelines::create(&c, NewPipeline { name: "X".into(), slug: "vendas".into(), description: None, stages: None, vocabulary: None });
    assert_eq!(dup.unwrap_err().status(), 409);
    assert_eq!(pipelines::list(&c).unwrap().len(), 1, "funil que falhou não deixou resto");
}

// ------------------------------------------------------------------ leads

#[test]
fn lead_goes_to_first_open_stage_of_default_pipeline() {
    let env = Env::new();
    let p = funnel(&env);
    let l = lead(&env, "Primeiro");
    assert_eq!(l.pipeline_id, p.pipeline.id);
    assert_eq!(l.stage_id, p.stages[0].id);
    assert_eq!(l.status, LeadStatus::Open);
    assert_eq!(l.last_activity_at, Some(NOW));

    let c = ctx!(env);
    let tl = activities::list(&c, &l.id, 10).unwrap();
    assert_eq!(tl.len(), 1);
    assert_eq!(tl[0].kind, "lead_created");
    assert_eq!(tl[0].performed_by.as_deref(), Some("user-1"));
}

#[test]
fn lead_without_pipeline_fails_clearly() {
    let env = Env::new();
    let c = ctx!(env);
    let e = leads::create(&c, NewLead { title: "x".into(), ..Default::default() }).unwrap_err();
    assert_eq!(code(e), "no_default_pipeline");
}

#[test]
fn lead_cannot_be_created_in_closed_stage_or_foreign_pipeline() {
    let env = Env::new();
    let p = funnel(&env);
    let c = ctx!(env);
    let won = &p.stages[3];
    let e = leads::create(&c, NewLead { title: "x".into(), stage_id: Some(won.id.clone()), ..Default::default() }).unwrap_err();
    assert_eq!(code(e), "stage_is_closed");

    let other = pipelines::create(&c, NewPipeline { name: "Pós".into(), slug: "pos".into(), description: None, stages: None, vocabulary: None }).unwrap();
    let e = leads::create(
        &c,
        NewLead { title: "x".into(), pipeline_id: Some(other.pipeline.id.clone()), stage_id: Some(p.stages[0].id.clone()), ..Default::default() },
    )
    .unwrap_err();
    assert_eq!(code(e), "stage_not_in_pipeline");
}

#[test]
fn new_leads_append_to_end_of_column() {
    let env = Env::new();
    let p = funnel(&env);
    for t in ["A", "B", "C"] {
        lead(&env, t);
    }
    assert_eq!(column(&env, &p.pipeline.id, "novo"), ["A", "B", "C"]);
}

#[test]
fn move_within_column_between_neighbors() {
    let env = Env::new();
    let p = funnel(&env);
    let a = lead(&env, "A");
    let _b = lead(&env, "B");
    let cc = lead(&env, "C");
    let c = ctx!(env);
    let novo = &p.stages[0].id;

    // C para o topo (só next)
    leads::move_lead(&c, &cc.id, MoveLead { stage_id: novo.clone(), next_lead_id: Some(a.id.clone()), ..Default::default() }).unwrap();
    assert_eq!(column(&env, &p.pipeline.id, "novo"), ["C", "A", "B"]);

    // C entre A e B (só prev)
    leads::move_lead(&c, &cc.id, MoveLead { stage_id: novo.clone(), prev_lead_id: Some(a.id.clone()), ..Default::default() }).unwrap();
    assert_eq!(column(&env, &p.pipeline.id, "novo"), ["A", "C", "B"]);

    // reordenar na mesma coluna não suja a timeline
    assert_eq!(activities::list(&c, &cc.id, 10).unwrap().len(), 1);
}

#[test]
fn move_to_other_column_records_stage_change() {
    let env = Env::new();
    let p = funnel(&env);
    let a = lead(&env, "A");
    let c = ctx!(env, NOW + 1000);
    let l = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[1].id.clone(), ..Default::default() }).unwrap();
    assert_eq!(l.stage_id, p.stages[1].id);
    assert_eq!(l.last_activity_at, Some(NOW + 1000));
    let tl = activities::list(&c, &a.id, 10).unwrap();
    assert_eq!(tl[0].kind, "stage_changed");
    assert_eq!(tl[0].payload["from"], p.stages[0].id.as_str());
}

#[test]
fn won_lost_and_reopen() {
    let env = Env::new();
    let p = funnel(&env);
    let a = lead(&env, "A");
    let c = ctx!(env, NOW + 5);

    let won = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[3].id.clone(), ..Default::default() }).unwrap();
    assert_eq!((won.status, won.closed_at), (LeadStatus::Won, Some(NOW + 5)));

    let e = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[4].id.clone(), ..Default::default() }).unwrap_err();
    assert_eq!(code(e), "lost_reason_required");
    let still = leads::get(&c, &a.id).unwrap();
    assert_eq!(still.status, LeadStatus::Won, "movimento recusado não mexeu em nada");

    let lost = leads::move_lead(
        &c,
        &a.id,
        MoveLead { stage_id: p.stages[4].id.clone(), lost_reason: Some("preço".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!((lost.status, lost.lost_reason.as_deref()), (LeadStatus::Lost, Some("preço")));

    let open = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[2].id.clone(), ..Default::default() }).unwrap();
    assert_eq!((open.status, open.closed_at, open.lost_reason), (LeadStatus::Open, None, None));

    let kinds: Vec<String> = activities::list(&c, &a.id, 20).unwrap().into_iter().map(|x| x.kind).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "status_changed").count(), 3);
}

#[test]
fn move_rejects_neighbor_from_other_column_and_self() {
    let env = Env::new();
    let p = funnel(&env);
    let a = lead(&env, "A");
    let b = lead(&env, "B");
    let c = ctx!(env);
    let e = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[1].id.clone(), prev_lead_id: Some(b.id.clone()), ..Default::default() });
    assert_eq!(code(e.unwrap_err()), "invalid_neighbor");
    let e = leads::move_lead(&c, &a.id, MoveLead { stage_id: p.stages[0].id.clone(), prev_lead_id: Some(a.id.clone()), ..Default::default() });
    assert_eq!(code(e.unwrap_err()), "invalid_neighbor");
}

#[test]
fn update_logs_fields_and_owner() {
    let env = Env::new();
    funnel(&env);
    let a = lead(&env, "A");
    let c = ctx!(env);
    let p: LeadPatch = serde_json::from_str(r#"{"title": "A2", "value_cents": 150000, "owner_user_id": "user-2"}"#).unwrap();
    let l = leads::update(&c, &a.id, p).unwrap();
    assert_eq!((l.title.as_str(), l.value_cents, l.owner_user_id.as_deref(), l.assigned_at), ("A2", Some(150000), Some("user-2"), Some(NOW)));
    let tl = activities::list(&c, &a.id, 10).unwrap();
    let kinds: Vec<&str> = tl.iter().map(|x| x.kind.as_str()).collect();
    assert!(kinds.contains(&"field_changed") && kinds.contains(&"owner_changed"));

    // PATCH sem mudança real não gera atividade
    let before = tl.len();
    leads::update(&c, &a.id, serde_json::from_str(r#"{"title": "A2"}"#).unwrap()).unwrap();
    assert_eq!(activities::list(&c, &a.id, 10).unwrap().len(), before);

    let e = leads::update(&c, &a.id, serde_json::from_str(r#"{"currency": "real"}"#).unwrap()).unwrap_err();
    assert_eq!(code(e), "invalid_currency");
}

#[test]
fn note_and_bad_contact_reference() {
    let env = Env::new();
    funnel(&env);
    let a = lead(&env, "A");
    let c = ctx!(env);
    leads::add_note(&c, &a.id, "ligar sexta").unwrap();
    assert_eq!(activities::list(&c, &a.id, 1).unwrap()[0].payload["body"], "ligar sexta");
    assert_eq!(code(leads::add_note(&c, &a.id, "   ").unwrap_err()), "empty_note");

    let e = leads::create(&c, NewLead { title: "x".into(), contact_id: Some("nao-existe".into()), ..Default::default() }).unwrap_err();
    assert_eq!(code(e), "invalid_reference");
}
