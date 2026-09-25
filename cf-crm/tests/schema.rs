//! Aplica as migrations num SQLite de verdade e prova as regras que o schema promete.

use crm_schema::model::MessageStatus;
use crm_schema::{service_window_open, GLOBAL_MIGRATIONS, SERVICE_WINDOW_MS, TENANT_MIGRATIONS};
use rusqlite::{params, Connection};

const T: i64 = 1_760_000_000_000;

fn tenant_db() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    for m in TENANT_MIGRATIONS {
        db.execute_batch(m.sql).unwrap_or_else(|e| panic!("migration {} falhou: {e}", m.name));
        db.pragma_update(None, "user_version", m.version).unwrap();
    }
    db
}

/// Contato + funil com duas etapas + conversa. Devolve o db pronto.
fn seeded() -> Connection {
    let db = tenant_db();
    db.execute_batch(&format!(
        "INSERT INTO contacts (id, wa_id, phone_e164, created_at, updated_at) VALUES ('c1', '5511987654321', '+5511987654321', {T}, {T});
         INSERT INTO pipelines (id, name, slug, position, created_at, updated_at) VALUES ('p1', 'Vendas', 'vendas', 'a0', {T}, {T});
         INSERT INTO pipelines (id, name, slug, position, created_at, updated_at) VALUES ('p2', 'Pós', 'pos', 'a1', {T}, {T});
         INSERT INTO stages (id, pipeline_id, name, slug, position, created_at, updated_at) VALUES ('s1', 'p1', 'Novo', 'novo', 'a0', {T}, {T});
         INSERT INTO stages (id, pipeline_id, name, slug, position, created_at, updated_at) VALUES ('s2', 'p2', 'Onboarding', 'onb', 'a0', {T}, {T});
         INSERT INTO conversations (id, contact_id, phone_number_id, status_changed_at, created_at, updated_at) VALUES ('cv1', 'c1', 'pn1', {T}, {T}, {T});"
    ))
    .unwrap();
    db
}

fn insert_lead(db: &Connection, id: &str, pipeline: &str, stage: &str, extra: &str) -> rusqlite::Result<usize> {
    db.execute(
        &format!(
            "INSERT INTO leads (id, pipeline_id, stage_id, contact_id, title, position_in_stage, created_at, updated_at{})
             VALUES (?1, ?2, ?3, 'c1', 'Lead', 'a0', {T}, {T}{})",
            if extra.is_empty() { "" } else { ", status, lost_reason, closed_at" },
            extra
        ),
        params![id, pipeline, stage],
    )
}

fn insert_inbound(db: &Connection, id: &str, wamid: &str) -> usize {
    db.execute(
        "INSERT INTO messages (id, conversation_id, contact_id, external_id, direction, type, status, body, sent_via, sent_at, created_at, updated_at)
         VALUES (?1, 'cv1', 'c1', ?2, 'inbound', 'text', 'received', 'oi', 'contact', ?3, ?3, ?3)
         ON CONFLICT (external_id) WHERE external_id IS NOT NULL DO NOTHING",
        params![id, wamid, T],
    )
    .unwrap()
}

fn status_of(db: &Connection, id: &str) -> String {
    db.query_row("SELECT status FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap()
}

#[test]
fn global_migrations_apply() {
    let db = Connection::open_in_memory().unwrap();
    for m in GLOBAL_MIGRATIONS {
        db.execute_batch(m.sql).unwrap();
    }
    let n: i64 = db
        .query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 6);
}

#[test]
fn tenant_migrations_are_versioned() {
    let db = tenant_db();
    let v: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(v, TENANT_MIGRATIONS.last().unwrap().version);
}

#[test]
fn webhook_replay_is_a_noop() {
    let db = seeded();
    assert_eq!(insert_inbound(&db, "m1", "wamid.A"), 1);
    assert_eq!(insert_inbound(&db, "m2", "wamid.A"), 0, "mesmo wamid entrou duas vezes");
}

#[test]
fn inbound_needs_wamid_and_received_status() {
    let db = seeded();
    let r = db.execute(
        &format!(
            "INSERT INTO messages (id, conversation_id, contact_id, direction, type, status, sent_via, sent_at, created_at, updated_at)
             VALUES ('m1', 'cv1', 'c1', 'inbound', 'text', 'received', 'contact', {T}, {T}, {T})"
        ),
        [],
    );
    assert!(r.is_err(), "inbound sem external_id foi aceito");

    let r = db.execute(
        &format!(
            "INSERT INTO messages (id, conversation_id, contact_id, external_id, direction, type, status, sent_via, sent_at, created_at, updated_at)
             VALUES ('m1', 'cv1', 'c1', 'w', 'outbound', 'text', 'received', 'crm', {T}, {T}, {T})"
        ),
        [],
    );
    assert!(r.is_err(), "outbound com status received foi aceito");
}

#[test]
fn outbound_queued_then_accepted_needs_wamid() {
    let db = seeded();
    db.execute(
        &format!(
            "INSERT INTO messages (id, conversation_id, contact_id, idempotency_key, direction, type, status, body, sent_via, sent_at, created_at, updated_at)
             VALUES ('o1', 'cv1', 'c1', 'k1', 'outbound', 'text', 'queued', 'olá', 'crm', {T}, {T}, {T})"
        ),
        [],
    )
    .unwrap();
    assert!(db.execute("UPDATE messages SET status = 'accepted' WHERE id = 'o1'", []).is_err());
    db.execute("UPDATE messages SET status = 'accepted', external_id = 'wamid.O' WHERE id = 'o1'", []).unwrap();
    assert_eq!(status_of(&db, "o1"), "accepted");

    // retry da Queue com a mesma chave não cria segunda mensagem
    let dup = db.execute(
        &format!(
            "INSERT INTO messages (id, conversation_id, contact_id, idempotency_key, direction, type, status, body, sent_via, sent_at, created_at, updated_at)
             VALUES ('o2', 'cv1', 'c1', 'k1', 'outbound', 'text', 'queued', 'olá', 'crm', {T}, {T}, {T})"
        ),
        [],
    );
    assert!(dup.is_err());
}

/// Para cada par (de, para), o trigger e `can_transition_to` têm de concordar.
#[test]
fn status_rule_matches_trigger() {
    let outbound: Vec<MessageStatus> = MessageStatus::ALL
        .iter()
        .copied()
        .filter(|s| *s != MessageStatus::Received)
        .collect();

    for &from in &outbound {
        for &to in &outbound {
            let db = seeded();
            let (wamid, failed_at) = (
                if from == MessageStatus::Queued { None } else { Some("wamid.X") },
                if from == MessageStatus::Failed { Some(T) } else { None },
            );
            db.execute(
                "INSERT INTO messages (id, conversation_id, contact_id, external_id, direction, type, status, body, sent_via, sent_at, failed_at, created_at, updated_at)
                 VALUES ('o', 'cv1', 'c1', ?1, 'outbound', 'text', ?2, 'x', 'crm', ?3, ?4, ?3, ?3)",
                params![wamid, from.as_str(), T, failed_at],
            )
            .unwrap();

            // o UPDATE traz o que o novo status exige, para só o trigger decidir
            let _ = db.execute(
                "UPDATE messages SET status = ?1, external_id = coalesce(external_id, 'wamid.X'), failed_at = coalesce(failed_at, ?2) WHERE id = 'o'",
                params![to.as_str(), T],
            );
            let applied = status_of(&db, "o") == to.as_str();
            assert_eq!(
                applied,
                from.can_transition_to(to),
                "{} -> {}: trigger {} , Rust {}",
                from.as_str(),
                to.as_str(),
                applied,
                from.can_transition_to(to)
            );
        }
    }
}

#[test]
fn late_delivered_does_not_undo_read() {
    let db = seeded();
    db.execute(
        &format!(
            "INSERT INTO messages (id, conversation_id, contact_id, external_id, direction, type, status, sent_via, sent_at, read_at, created_at, updated_at)
             VALUES ('o', 'cv1', 'c1', 'wamid.R', 'outbound', 'text', 'read', 'crm', {T}, {T}, {T}, {T})"
        ),
        [],
    )
    .unwrap();
    let changed = db
        .execute("UPDATE messages SET status = 'delivered', delivered_at = 1 WHERE external_id = 'wamid.R'", [])
        .unwrap();
    assert_eq!(changed, 0);
    assert_eq!(status_of(&db, "o"), "read");
}

#[test]
fn lead_stage_must_belong_to_lead_pipeline() {
    let db = seeded();
    insert_lead(&db, "l1", "p1", "s1", "").unwrap();
    assert!(insert_lead(&db, "l2", "p1", "s2", "").is_err(), "etapa de outro funil foi aceita");
}

#[test]
fn lost_lead_needs_reason_and_closed_at() {
    let db = seeded();
    assert!(insert_lead(&db, "l1", "p1", "s1", &format!(", 'lost', NULL, {T}")).is_err());
    assert!(insert_lead(&db, "l2", "p1", "s1", ", 'lost', 'preço', NULL").is_err());
    assert!(insert_lead(&db, "l3", "p1", "s1", &format!(", 'lost', 'preço', {T}")).is_ok());
    assert!(insert_lead(&db, "l4", "p1", "s1", &format!(", 'open', NULL, {T}")).is_err());
}

#[test]
fn contact_is_never_cascaded_away() {
    let db = seeded();
    insert_lead(&db, "l1", "p1", "s1", "").unwrap();
    insert_inbound(&db, "m1", "wamid.A");
    assert!(db.execute("DELETE FROM contacts WHERE id = 'c1'", []).is_err());
}

#[test]
fn identity_is_unique_only_among_live_contacts() {
    let db = seeded();
    let dup = db.execute(
        &format!("INSERT INTO contacts (id, wa_id, created_at, updated_at) VALUES ('c2', '5511987654321', {T}, {T})"),
        [],
    );
    assert!(dup.is_err(), "wa_id duplicado entre contatos vivos");

    db.execute_batch(&format!(
        "INSERT INTO contacts (id, created_at, updated_at) VALUES ('c3', {T}, {T});
         UPDATE contacts SET merged_into_id = 'c3', merged_at = {T} WHERE id = 'c1';
         INSERT INTO contacts (id, wa_id, created_at, updated_at) VALUES ('c2', '5511987654321', {T}, {T});"
    ))
    .expect("contato mesclado ainda bloqueia o wa_id");
}

#[test]
fn only_one_default_pipeline() {
    let db = seeded();
    db.execute("UPDATE pipelines SET is_default = 1 WHERE id = 'p1'", []).unwrap();
    assert!(db.execute("UPDATE pipelines SET is_default = 1 WHERE id = 'p2'", []).is_err());
}

#[test]
fn json_columns_reject_garbage() {
    let db = seeded();
    assert!(db.execute("UPDATE contacts SET consent = '{nope' WHERE id = 'c1'", []).is_err());
}

#[test]
fn phone_must_be_e164() {
    let db = tenant_db();
    for bad in ["11987654321", "+55 11 98765-4321", "+0123456789"] {
        let r = db.execute(
            &format!("INSERT INTO contacts (id, phone_e164, created_at, updated_at) VALUES ('x', ?1, {T}, {T})"),
            [bad],
        );
        assert!(r.is_err(), "{bad} foi aceito como E.164");
    }
}

#[test]
fn webhook_retry_is_deduped_by_idempotency_key() {
    let db = tenant_db();
    let receive = |key: &str| {
        db.execute(
            "INSERT INTO webhook_receipts (idempotency_key, event, received_at) VALUES (?1, 'whatsapp.message.delivered', ?2)
             ON CONFLICT DO NOTHING",
            params![key, T],
        )
        .unwrap()
    };
    assert_eq!(receive("k-1"), 1);
    assert_eq!(receive("k-1"), 0, "retry do Kapso foi processado de novo");
    assert_eq!(receive("k-2"), 1);
}

#[test]
fn service_window_is_24h() {
    assert!(service_window_open(Some(T), T + SERVICE_WINDOW_MS - 1));
    assert!(!service_window_open(Some(T), T + SERVICE_WINDOW_MS));
    assert!(!service_window_open(None, T));
}
