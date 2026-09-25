//! Fase 3: webhook do Kapso -> DO, envio pela outbox. SQLite de verdade, payloads
//! no formato v2 da documentação do Kapso.
mod common;

use common::{Env, NOW};
use crm_core::contacts::{self, NewContact};
use crm_core::inbox::{self, Ingested, ADOPT_UNKNOWN_OUTBOUND_AFTER_MS};
use crm_core::messaging::{self, OpenConversation, Outgoing};
use crm_core::outbox::{self, SendOutcome, IN_FLIGHT_LEASE_MS, MAX_ATTEMPTS};
use crm_schema::kapso::{EventPayload, EVENT_MESSAGE_DELIVERED, EVENT_MESSAGE_FAILED, EVENT_MESSAGE_READ, EVENT_MESSAGE_RECEIVED, EVENT_MESSAGE_SENT};
use crm_schema::model::{MessageStatus, SentVia};
use serde_json::{json, Value};

const PNID: &str = "123456789012345";
const PHONE: &str = "+5511987654321";

fn payload(v: Value) -> EventPayload {
    serde_json::from_value(v).unwrap()
}

/// `whatsapp.message.received` no formato v2 (docs do Kapso, "Event types").
fn received(wamid: &str, text: &str, ts_s: i64) -> EventPayload {
    payload(json!({
        "message": {
            "id": wamid, "timestamp": ts_s.to_string(), "type": "text", "text": { "body": text },
            "kapso": { "direction": "inbound", "status": "received", "origin": "cloud_api", "has_media": false, "content": text }
        },
        "conversation": {
            "id": "conv_1", "phone_number": PHONE, "status": "active", "phone_number_id": PNID,
            "kapso": { "contact_name": "Ana Souza" }
        },
        "is_new_conversation": true,
        "phone_number_id": PNID
    }))
}

/// Evento de status de saída. `statuses` com callback data opcional.
fn outbound_status(wamid: &str, status: &str, ts_s: i64, origin: &str, callback: Option<&str>, errors: Option<Value>) -> EventPayload {
    let mut st = json!({ "id": wamid, "status": status, "timestamp": ts_s.to_string(), "recipient_id": "5511987654321" });
    if let Some(cb) = callback {
        st["biz_opaque_callback_data"] = json!(cb);
    }
    if let Some(e) = errors {
        st["errors"] = e;
    }
    payload(json!({
        "message": {
            "id": wamid, "timestamp": ts_s.to_string(), "type": "text", "text": { "body": "resposta" },
            "kapso": { "direction": "outbound", "status": status, "origin": origin, "has_media": false, "statuses": [st] }
        },
        "conversation": { "id": "conv_1", "phone_number": PHONE, "phone_number_id": PNID },
        "is_new_conversation": false,
        "phone_number_id": PNID
    }))
}

fn secs(ms: i64) -> i64 {
    ms / 1000
}

fn ingest(env: &Env, now: i64, event: &str, key: &str, p: &EventPayload) -> Ingested {
    inbox::ingest(&env.system(now), event, key, p).unwrap()
}

fn count(env: &Env, sql: &str) -> i64 {
    env.db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Conversa com o contato já falando (janela aberta) e a mensagem de saída na fila.
fn conversation_with_inbound(env: &Env) -> String {
    let r = ingest(env, NOW, EVENT_MESSAGE_RECEIVED, "k-in", &received("wamid.in1", "oi, quero saber o preço", secs(NOW)));
    let Ingested::Stored { message_id } = r else { panic!("{r:?}") };
    messaging::get_message(&env.system(NOW), &message_id).unwrap().conversation_id
}

fn send_text(env: &Env, conv: &str, body: &str) -> String {
    let c = env.ctx(NOW);
    messaging::send(&c, conv, Outgoing::Text { body: body.into() }, None).unwrap().message.id
}

fn status_of(env: &Env, id: &str) -> MessageStatus {
    messaging::get_message(&env.system(NOW), id).unwrap().status
}

// ------------------------------------------------------------------ entrada

#[test]
fn inbound_creates_contact_conversation_and_message() {
    let env = Env::new();
    let conv_id = conversation_with_inbound(&env);
    let c = env.system(NOW);

    let conv = messaging::get_conversation(&c, &conv_id).unwrap();
    assert_eq!(conv.phone_number_id, PNID);
    assert_eq!(conv.kapso_conversation_id.as_deref(), Some("conv_1"));
    assert_eq!(conv.unread_count, 1);
    assert_eq!(conv.last_inbound_at, Some(secs(NOW) * 1000));
    assert_eq!(conv.last_message_preview.as_deref(), Some("oi, quero saber o preço"));
    assert!(conv.service_window_open(NOW));

    let contact = contacts::get(&c, &conv.contact_id).unwrap();
    assert_eq!(contact.wa_id.as_deref(), Some("5511987654321"));
    assert_eq!(contact.phone_e164.as_deref(), Some(PHONE));
    assert_eq!(contact.display_name.as_deref(), Some("Ana Souza"));
    assert_eq!(contact.source, "whatsapp");

    let page = messaging::list_messages(&c, &conv_id, 10, None).unwrap();
    assert_eq!(page.items.len(), 1);
    let m = &page.items[0];
    assert_eq!(m.status, MessageStatus::Received);
    assert_eq!(m.sent_via, SentVia::Contact);
    assert_eq!(m.external_id.as_deref(), Some("wamid.in1"));
    assert_eq!(m.metadata["kapso_origin"], "cloud_api");
}

#[test]
fn same_delivery_twice_is_a_noop() {
    let env = Env::new();
    let p = received("wamid.x", "olá", secs(NOW));
    assert!(matches!(ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "key-1", &p), Ingested::Stored { .. }));
    assert_eq!(ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "key-1", &p), Ingested::Duplicate);
    assert_eq!(count(&env, "SELECT count(*) FROM messages"), 1);
}

#[test]
fn same_wamid_with_new_key_is_a_noop_and_does_not_count_twice() {
    // Lote que falhou é reentregue item a item, cada um com chave nova.
    let env = Env::new();
    let p = received("wamid.x", "olá", secs(NOW));
    ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "lote:0", &p);
    assert_eq!(ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "avulso-novo", &p), Ingested::Duplicate);
    assert_eq!(count(&env, "SELECT count(*) FROM messages"), 1);
    assert_eq!(count(&env, "SELECT unread_count FROM conversations"), 1);
}

#[test]
fn late_message_does_not_overwrite_preview() {
    let env = Env::new();
    ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k2", &received("wamid.2", "segunda", secs(NOW) + 10));
    ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k1", &received("wamid.1", "primeira", secs(NOW)));
    assert_eq!(count(&env, "SELECT count(*) FROM conversations"), 1);
    let preview: String = env.db.conn.query_row("SELECT last_message_preview FROM conversations", [], |r| r.get(0)).unwrap();
    assert_eq!(preview, "segunda");
    let c = env.system(NOW);
    let conv: String = env.db.conn.query_row("SELECT id FROM conversations", [], |r| r.get(0)).unwrap();
    let bodies: Vec<_> = messaging::list_messages(&c, &conv, 10, None).unwrap().items.into_iter().map(|m| m.body.unwrap()).collect();
    assert_eq!(bodies, ["segunda", "primeira"], "ordem pela hora da Meta, não pela chegada");
}

#[test]
fn inbound_finds_contact_registered_by_phone() {
    let env = Env::new();
    let manual = contacts::create(&env.ctx(NOW), NewContact { name: Some("Ana".into()), phone: Some(PHONE.into()), ..Default::default() }).unwrap();
    conversation_with_inbound(&env);
    assert_eq!(count(&env, "SELECT count(*) FROM contacts"), 1);
    let c = contacts::get(&env.system(NOW), &manual.id).unwrap();
    assert_eq!(c.wa_id.as_deref(), Some("5511987654321"), "wa_id gravado no contato que já existia");
    assert_eq!(c.name.as_deref(), Some("Ana"), "nome dado pela equipe continua");
}

#[test]
fn wa_id_without_ninth_digit_does_not_steal_other_contact() {
    // Contato novo cujo telefone já pertence a outro contato com wa_id diferente.
    let env = Env::new();
    conversation_with_inbound(&env);
    env.db.conn.execute("UPDATE contacts SET wa_id = '551187654321'", []).unwrap();
    ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k-9", &received("wamid.9", "oi de novo", secs(NOW)));
    assert_eq!(count(&env, "SELECT count(*) FROM contacts"), 2);
    assert_eq!(count(&env, "SELECT count(*) FROM contacts WHERE phone_e164 IS NULL AND wa_id = '5511987654321'"), 1);
}

#[test]
fn media_message_keeps_caption_mime_and_media_id() {
    let env = Env::new();
    let p = payload(json!({
        "message": {
            "id": "wamid.img", "timestamp": secs(NOW).to_string(), "type": "image",
            "image": { "caption": "comprovante", "id": "media_id_123" },
            "kapso": {
                "direction": "inbound", "status": "received", "origin": "cloud_api", "has_media": true,
                "media_url": "https://api.kapso.ai/media/x",
                "media_data": { "url": "https://api.kapso.ai/media/x", "filename": "photo.jpg", "content_type": "image/jpeg", "byte_size": 204800 }
            }
        },
        "conversation": { "id": "conv_1", "phone_number": PHONE, "phone_number_id": PNID },
        "phone_number_id": PNID
    }));
    let Ingested::Stored { message_id } = ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k", &p) else { panic!() };
    let m = messaging::get_message(&env.system(NOW), &message_id).unwrap();
    assert_eq!(m.kind.as_str(), "image");
    assert_eq!(m.body.as_deref(), Some("comprovante"));
    assert_eq!(m.media_meta_id.as_deref(), Some("media_id_123"));
    assert_eq!(m.media_mime.as_deref(), Some("image/jpeg"));
    assert_eq!(m.media_size_bytes, Some(204800));
    assert_eq!(m.metadata["kapso_media_url"], "https://api.kapso.ai/media/x");
}

#[test]
fn unknown_type_is_stored_as_unsupported() {
    let env = Env::new();
    let p = payload(json!({
        "message": { "id": "wamid.u", "timestamp": secs(NOW).to_string(), "type": "order", "order": {},
                     "kapso": { "direction": "inbound", "origin": "cloud_api" } },
        "conversation": { "id": "conv_1", "phone_number": PHONE },
        "phone_number_id": PNID
    }));
    let Ingested::Stored { message_id } = ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k", &p) else { panic!() };
    let m = messaging::get_message(&env.system(NOW), &message_id).unwrap();
    assert_eq!(m.kind.as_str(), "unsupported");
    assert_eq!(m.metadata["wa_type"], "order");
}

#[test]
fn history_sync_does_not_count_as_unread() {
    let env = Env::new();
    let mut p = received("wamid.h", "mensagem antiga", secs(NOW) - 86_400 * 30);
    p.message.as_mut().unwrap().kapso.as_mut().unwrap().origin = Some("history_sync".into());
    ingest(&env, NOW, EVENT_MESSAGE_RECEIVED, "k", &p);
    assert_eq!(count(&env, "SELECT unread_count FROM conversations"), 0);
}

#[test]
fn closed_conversation_reopens_on_inbound() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    env.db.conn.execute("UPDATE conversations SET status = 'closed'", []).unwrap();
    ingest(&env, NOW + 5_000, EVENT_MESSAGE_RECEIVED, "k2", &received("wamid.2", "voltei", secs(NOW) + 5));
    assert_eq!(messaging::get_conversation(&env.system(NOW), &conv).unwrap().status.as_str(), "open");
}

#[test]
fn other_events_are_recorded_and_ignored() {
    let env = Env::new();
    let p = payload(json!({ "phone_number_id": PNID, "conversation": { "id": "conv_1" } }));
    assert!(matches!(ingest(&env, NOW, "whatsapp.conversation.ended", "k", &p), Ingested::Ignored { .. }));
    assert_eq!(ingest(&env, NOW, "whatsapp.conversation.ended", "k", &p), Ingested::Duplicate);
}

// ------------------------------------------------------------------ envio

#[test]
fn send_text_queues_message_and_outbox_atomically() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "custa R$ 1.500");

    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Queued);
    assert_eq!(m.sent_via, SentVia::Crm);
    assert_eq!(m.sent_by_user_id.as_deref(), Some("user-1"));
    assert!(m.external_id.is_none());
    assert_eq!(count(&env, "SELECT count(*) FROM outbox WHERE kind = 'send_message'"), 1);
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.last_message_preview.as_deref(), Some("custa R$ 1.500"));
    assert!(c.last_outbound_at.is_some());
}

#[test]
fn text_outside_24h_window_is_refused_but_template_goes() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let later = env.ctx(NOW + crm_schema::SERVICE_WINDOW_MS + 1_000);
    let e = messaging::send(&later, &conv, Outgoing::Text { body: "oi".into() }, None).unwrap_err();
    assert_eq!(e.code(), "service_window_closed");
    assert_eq!(count(&env, "SELECT count(*) FROM outbox"), 0, "recusa não deixa resto");

    let t = Outgoing::Template {
        name: "retomar_contato".into(),
        language: "pt_BR".into(),
        components: Some(json!([{ "type": "body", "parameters": [{ "type": "text", "text": "Ana" }] }])),
    };
    let sent = messaging::send(&later, &conv, t, None).unwrap();
    assert_eq!(sent.message.kind.as_str(), "template");
}

#[test]
fn send_validates_input() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let c = env.ctx(NOW);
    let code = |o: Outgoing| messaging::send(&c, &conv, o, None).unwrap_err().code();
    assert_eq!(code(Outgoing::Text { body: "   ".into() }), "empty_message");
    assert_eq!(code(Outgoing::Text { body: "a".repeat(4097) }), "message_too_long");
    assert_eq!(code(Outgoing::Template { name: "Com Espaço".into(), language: "pt_BR".into(), components: None }), "invalid_template");
    assert_eq!(code(Outgoing::Template { name: "ok".into(), language: "pt_BR".into(), components: Some(json!({})) }), "invalid_template");
    assert_eq!(messaging::send(&c, "nao-existe", Outgoing::Text { body: "x".into() }, None).unwrap_err().status(), 404);
}

#[test]
fn blocked_contact_does_not_receive() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    env.db.conn.execute("UPDATE contacts SET is_blocked = 1, blocked_at = 1, blocked_reason = 'opt_out'", []).unwrap();
    let e = messaging::send(&env.ctx(NOW), &conv, Outgoing::Text { body: "oi".into() }, None).unwrap_err();
    assert_eq!(e.code(), "contact_blocked");
}

#[test]
fn idempotency_key_returns_the_same_message() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let c = env.ctx(NOW);
    let a = messaging::send(&c, &conv, Outgoing::Text { body: "uma vez".into() }, Some("chave-1")).unwrap();
    let b = messaging::send(&c, &conv, Outgoing::Text { body: "uma vez".into() }, Some("chave-1")).unwrap();
    assert!(a.created && !b.created);
    assert_eq!(a.message.id, b.message.id);
    assert_eq!(count(&env, "SELECT count(*) FROM outbox"), 1);
}

#[test]
fn open_conversation_for_outreach_needs_template() {
    let env = Env::new();
    let c = env.ctx(NOW);
    let contact = contacts::create(&c, NewContact { name: Some("Bia".into()), phone: Some("+351912345678".into()), ..Default::default() }).unwrap();
    let conv = messaging::open(&c, OpenConversation { contact_id: contact.id.clone(), phone_number_id: PNID.into() }).unwrap();
    let again = messaging::open(&c, OpenConversation { contact_id: contact.id, phone_number_id: PNID.into() }).unwrap();
    assert_eq!(conv.id, again.id);
    assert_eq!(messaging::send(&c, &conv.id, Outgoing::Text { body: "oi".into() }, None).unwrap_err().code(), "service_window_closed");
    let only_email = contacts::create(&c, NewContact { email: Some("x@y.com".into()), ..Default::default() }).unwrap();
    let e = messaging::open(&c, OpenConversation { contact_id: only_email.id, phone_number_id: PNID.into() }).unwrap_err();
    assert_eq!(e.code(), "contact_without_phone");
}

// ------------------------------------------------------------------ outbox

#[test]
fn claim_builds_cloud_api_request_and_marks_in_flight() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "custa R$ 1.500");

    let jobs = outbox::claim_due(&env.system(NOW + 2_000), 10).unwrap();
    assert_eq!(jobs.len(), 1);
    let j = &jobs[0];
    assert_eq!(j.message_id, id);
    assert_eq!(j.phone_number_id, PNID);
    assert_eq!(
        j.body,
        json!({
            "messaging_product": "whatsapp", "recipient_type": "individual", "to": "5511987654321",
            "biz_opaque_callback_data": id, "type": "text", "text": { "body": "custa R$ 1.500", "preview_url": false }
        })
    );
    assert!(outbox::claim_due(&env.system(NOW + 3_000), 10).unwrap().is_empty(), "em andamento não sai de novo");
}

#[test]
fn template_request_carries_components() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let t = Outgoing::Template { name: "boas_vindas".into(), language: "pt_BR".into(), components: Some(json!([{ "type": "body", "parameters": [] }])) };
    messaging::send(&env.ctx(NOW), &conv, t, None).unwrap();
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    assert_eq!(j.body["type"], "template");
    assert_eq!(j.body["template"], json!({ "name": "boas_vindas", "language": { "code": "pt_BR" }, "components": [{ "type": "body", "parameters": [] }] }));
}

#[test]
fn accepted_records_wamid_and_clears_outbox() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    outbox::complete(&env.system(NOW + 500), &j, &SendOutcome::Accepted { wamid: "wamid.out1".into() }).unwrap();

    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Accepted);
    assert_eq!(m.external_id.as_deref(), Some("wamid.out1"));
    assert_eq!(count(&env, "SELECT count(*) FROM outbox"), 0);
    assert_eq!(outbox::next_wake_at(&env.system(NOW)).unwrap(), None);
}

#[test]
fn temporary_error_backs_off_then_gives_up() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let busy = SendOutcome::Rejected { code: "http_503".into(), message: "fora do ar".into(), retryable: true };

    let mut now = NOW;
    for attempt in 1..=MAX_ATTEMPTS {
        let jobs = outbox::claim_due(&env.system(now), 10).unwrap();
        assert_eq!(jobs.len(), 1, "tentativa {attempt}");
        outbox::complete(&env.system(now), &jobs[0], &busy).unwrap();
        if attempt < MAX_ATTEMPTS {
            assert_eq!(status_of(&env, &id), MessageStatus::Queued);
            let wake = outbox::next_wake_at(&env.system(now)).unwrap().unwrap();
            assert_eq!(wake - now, outbox::backoff_ms(attempt));
            assert!(outbox::claim_due(&env.system(wake - 1), 10).unwrap().is_empty(), "não sai antes da hora");
            now = wake;
        }
    }
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Failed);
    assert_eq!(m.error_code.as_deref(), Some("http_503"));
    assert_eq!(count(&env, "SELECT count(*) FROM outbox"), 0);
}

#[test]
fn permanent_error_fails_at_once() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    let bad = outbox::classify(400, &json!({ "error": { "code": 131047, "message": "Re-engagement message" } }));
    outbox::complete(&env.system(NOW), &j, &bad).unwrap();
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Failed);
    assert_eq!(m.error_code.as_deref(), Some("131047"));
    assert_eq!(m.error_message.as_deref(), Some("Re-engagement message"));
}

#[test]
fn lost_response_fails_instead_of_sending_twice() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    // o DO caiu: complete() nunca rodou
    assert_eq!(outbox::next_wake_at(&env.system(NOW)).unwrap(), Some(NOW + IN_FLIGHT_LEASE_MS));
    let jobs = outbox::claim_due(&env.system(NOW + IN_FLIGHT_LEASE_MS), 10).unwrap();
    assert!(jobs.is_empty(), "não reenvia às cegas");
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Failed);
    assert_eq!(m.error_code.as_deref(), Some("send_outcome_unknown"));
    // resposta atrasada de uma rodada antiga não ressuscita nada
    outbox::complete(&env.system(NOW + IN_FLIGHT_LEASE_MS + 1), &j, &SendOutcome::Accepted { wamid: "w".into() }).unwrap();
    assert_eq!(status_of(&env, &id), MessageStatus::Failed);
}

#[test]
fn lost_response_but_webhook_confirmed_is_not_failed() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    outbox::claim_due(&env.system(NOW), 10).unwrap();
    let st = outbound_status("wamid.cb", "sent", secs(NOW), "cloud_api", Some(&id), None);
    ingest(&env, NOW + 1_000, EVENT_MESSAGE_SENT, "k-s", &st);
    outbox::claim_due(&env.system(NOW + IN_FLIGHT_LEASE_MS), 10).unwrap();
    assert_eq!(status_of(&env, &id), MessageStatus::Sent);
    assert_eq!(count(&env, "SELECT count(*) FROM outbox"), 0);
}

#[test]
fn classify_responses() {
    use SendOutcome::*;
    let ok = json!({ "messaging_product": "whatsapp", "contacts": [{ "input": "5511", "wa_id": "5511" }], "messages": [{ "id": "wamid.A" }] });
    assert_eq!(outbox::classify(200, &ok), Accepted { wamid: "wamid.A".into() });
    assert!(matches!(outbox::classify(200, &json!({})), Rejected { retryable: false, .. }), "200 sem id pode ter saído: não repetir");
    assert!(matches!(outbox::classify(0, &Value::Null), Rejected { retryable: true, .. }));
    assert!(matches!(outbox::classify(429, &Value::Null), Rejected { retryable: true, .. }));
    assert!(matches!(outbox::classify(502, &Value::Null), Rejected { retryable: true, .. }));
    assert!(matches!(outbox::classify(400, &json!({ "error": { "code": 131056 } })), Rejected { retryable: true, .. }), "limite por par");
    assert!(matches!(outbox::classify(401, &json!({ "error": { "message": "chave inválida" } })), Rejected { retryable: false, .. }));
}

// ------------------------------------------------------------------ status

fn accepted(env: &Env) -> String {
    let conv = conversation_with_inbound(env);
    let id = send_text(env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    outbox::complete(&env.system(NOW), &j, &SendOutcome::Accepted { wamid: "wamid.out".into() }).unwrap();
    id
}

#[test]
fn statuses_move_forward_and_ignore_late_ones() {
    let env = Env::new();
    let id = accepted(&env);
    let ev = |status: &str, key: &str, event: &str| ingest(&env, NOW, event, key, &outbound_status("wamid.out", status, secs(NOW), "cloud_api", None, None));

    assert_eq!(ev("read", "k1", EVENT_MESSAGE_READ), Ingested::StatusUpdated { message_id: id.clone() });
    ev("delivered", "k2", EVENT_MESSAGE_DELIVERED);
    ev("sent", "k3", EVENT_MESSAGE_SENT);
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Read, "read chegou primeiro e ficou");
    assert!(m.read_at.is_some() && m.delivered_at.is_some(), "read implica entregue");
}

#[test]
fn failed_status_records_meta_error() {
    let env = Env::new();
    let id = accepted(&env);
    let errors = json!([{ "code": 131047, "title": "Re-engagement message", "message": "More than 24 hours have passed" }]);
    ingest(&env, NOW, EVENT_MESSAGE_FAILED, "k", &outbound_status("wamid.out", "failed", secs(NOW), "cloud_api", None, Some(errors)));
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Failed);
    assert_eq!(m.error_code.as_deref(), Some("131047"));
    assert_eq!(m.error_message.as_deref(), Some("More than 24 hours have passed"));
    assert!(m.failed_at.is_some());
}

#[test]
fn status_before_send_response_is_matched_by_callback_data() {
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);

    // o webhook chega antes de complete()
    let st = outbound_status("wamid.fast", "delivered", secs(NOW), "cloud_api", Some(&id), None);
    assert_eq!(ingest(&env, NOW, EVENT_MESSAGE_DELIVERED, "k", &st), Ingested::StatusUpdated { message_id: id.clone() });
    outbox::complete(&env.system(NOW + 1), &j, &SendOutcome::Accepted { wamid: "wamid.fast".into() }).unwrap();

    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!(m.status, MessageStatus::Delivered, "accepted depois de delivered não regride");
    assert_eq!(m.external_id.as_deref(), Some("wamid.fast"));
    assert_eq!(count(&env, "SELECT count(*) FROM messages WHERE direction = 'outbound'"), 1);
}

#[test]
fn unknown_api_outbound_waits_then_is_adopted() {
    let env = Env::new();
    conversation_with_inbound(&env);
    let st = outbound_status("wamid.alheio", "sent", secs(NOW), "cloud_api", None, None);
    let e = inbox::ingest(&env.system(NOW + 1_000), EVENT_MESSAGE_SENT, "k", &st).unwrap_err();
    assert_eq!((e.status(), e.code()), (503, "outbound_not_yet_known"));
    assert_eq!(count(&env, "SELECT count(*) FROM webhook_receipts WHERE idempotency_key = 'k'"), 0, "recibo desfeito: a fila tenta de novo");

    let later = NOW + ADOPT_UNKNOWN_OUTBOUND_AFTER_MS + 1_000;
    let Ingested::Stored { message_id } = ingest(&env, later, EVENT_MESSAGE_SENT, "k", &st) else { panic!() };
    let m = messaging::get_message(&env.system(NOW), &message_id).unwrap();
    assert_eq!((m.sent_via, m.status), (SentVia::Api, MessageStatus::Sent));
}

#[test]
fn business_app_echo_is_stored_at_once() {
    let env = Env::new();
    conversation_with_inbound(&env);
    let st = outbound_status("wamid.app", "sent", secs(NOW), "business_app", None, None);
    let Ingested::Stored { message_id } = ingest(&env, NOW, EVENT_MESSAGE_SENT, "k", &st) else { panic!() };
    let m = messaging::get_message(&env.system(NOW), &message_id).unwrap();
    assert_eq!(m.sent_via, SentVia::ExternalDevice);
    assert_eq!(count(&env, "SELECT count(*) FROM conversations"), 1, "mesma conversa do contato");
}

#[test]
fn adopted_copy_is_merged_when_send_response_arrives() {
    // Sem callback data, o status adotou a mensagem como de outra origem; a
    // resposta do envio chega depois e prova que era nossa.
    let env = Env::new();
    let conv = conversation_with_inbound(&env);
    let id = send_text(&env, &conv, "oi");
    let j = outbox::claim_due(&env.system(NOW), 10).unwrap().remove(0);
    let later = NOW + ADOPT_UNKNOWN_OUTBOUND_AFTER_MS + 1_000;
    ingest(&env, later, EVENT_MESSAGE_DELIVERED, "k", &outbound_status("wamid.slow", "delivered", secs(NOW), "cloud_api", None, None));
    assert_eq!(count(&env, "SELECT count(*) FROM messages WHERE direction = 'outbound'"), 2);

    outbox::complete(&env.system(later + 1), &j, &SendOutcome::Accepted { wamid: "wamid.slow".into() }).unwrap();
    assert_eq!(count(&env, "SELECT count(*) FROM messages WHERE direction = 'outbound'"), 1);
    let m = messaging::get_message(&env.system(NOW), &id).unwrap();
    assert_eq!((m.status, m.external_id.as_deref()), (MessageStatus::Delivered, Some("wamid.slow")));
}

#[test]
fn inbox_lists_by_last_message_with_window_flag() {
    let env = Env::new();
    conversation_with_inbound(&env);
    let c = env.ctx(NOW);
    let other = contacts::create(&c, NewContact { name: Some("Bia".into()), phone: Some("+351912345678".into()), ..Default::default() }).unwrap();
    messaging::open(&env.ctx(NOW + 60_000), OpenConversation { contact_id: other.id, phone_number_id: PNID.into() }).unwrap();

    let page = messaging::list_conversations(&c, None, 1, None).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].contact_name.as_deref(), Some("Bia"));
    assert!(!page.items[0].service_window_open);
    let next = messaging::list_conversations(&c, None, 1, page.next_cursor.as_deref()).unwrap();
    assert_eq!(next.items[0].contact_name.as_deref(), Some("Ana Souza"));
    assert!(next.items[0].service_window_open);
    assert!(next.next_cursor.is_none());

    assert_eq!(messaging::list_conversations(&c, Some("xyz"), 10, None).unwrap_err().code(), "invalid_status");
    let read = messaging::mark_read(&c, &next.items[0].conversation.id).unwrap();
    assert_eq!(read.unread_count, 0);
}
