use crm_schema::kapso::verify_signature;

const SECRET: &[u8] = b"segredo-do-webhook";
const BODY: &[u8] = br#"{"type":"whatsapp.message.received"}"#;
// Calculado fora do Rust: python3 hmac.new(SECRET, BODY, sha256).hexdigest()
const SIG: &str = "66f325f1396ffd7390c4feb64b1bfaea776c1e20ee72b7919618d4faea640504";

#[test]
fn accepts_valid_signature() {
    assert!(verify_signature(SECRET, BODY, SIG));
    assert!(verify_signature(SECRET, BODY, &SIG.to_uppercase()));
}

#[test]
fn rejects_tampered_body_wrong_secret_and_garbage() {
    assert!(!verify_signature(SECRET, br#"{"type":"whatsapp.message.sent"}"#, SIG));
    assert!(!verify_signature(b"outro", BODY, SIG));
    assert!(!verify_signature(SECRET, BODY, "nao-e-hex"));
    assert!(!verify_signature(SECRET, BODY, ""));
    assert!(!verify_signature(SECRET, BODY, &SIG[..32]));
}

// ------------------------------------------------------------------ payloads v2 (tests/fixtures/kapso)

use crm_schema::kapso::{split_delivery, EventPayload, WaError};
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/kapso/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn parse(v: Value) -> EventPayload {
    serde_json::from_value(v).unwrap()
}

#[test]
fn documented_payloads_parse() {
    for name in ["message_received.json", "message_sent.json", "message_delivered.json", "message_failed.json"] {
        let p = parse(fixture(name));
        assert_eq!(p.phone_number_id, "123456789012345", "{name}");
        assert!(p.message.is_some(), "{name}");
    }
    let r = parse(fixture("message_received.json"));
    let m = r.message.unwrap();
    assert_eq!(m.timestamp_ms(), Some(1_730_092_800_000));
    assert_eq!(m.display_text().as_deref(), Some("Hello"));
    assert_eq!(m.meta().direction.as_deref(), Some("inbound"));
    let conv = r.conversation.unwrap();
    assert_eq!(conv.phone_number.as_deref(), Some("+15551234567"));
    assert_eq!(conv.kapso.unwrap().contact_name.as_deref(), Some("John Doe"));
}

#[test]
fn failed_payload_exposes_meta_error() {
    let m = parse(fixture("message_failed.json")).message.unwrap();
    assert_eq!(
        m.last_error(),
        Some(WaError { code: "131047".into(), message: "More than 24 hours have passed since the recipient last replied".into() })
    );
    assert_eq!(parse(fixture("message_sent.json")).message.unwrap().last_error(), None);
}

#[test]
fn batch_is_split_by_the_batch_field() {
    let events = split_delivery(None, fixture("batch_received.json"));
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|(e, _)| e == "whatsapp.message.received"), "nome do evento vem do corpo do lote");
    let ids: Vec<String> = events.into_iter().map(|(_, p)| parse(p).message.unwrap().id).collect();
    assert_eq!(ids, ["wamid.111", "wamid.112"]);
}

#[test]
fn single_delivery_uses_the_header_event() {
    let events = split_delivery(Some("whatsapp.message.sent"), fixture("message_sent.json"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "whatsapp.message.sent");
    assert!(split_delivery(None, serde_json::json!({ "batch": true })).is_empty(), "lote sem data não inventa evento");
}

#[test]
fn callback_data_comes_from_statuses() {
    let mut v = fixture("message_delivered.json");
    v["message"]["kapso"]["statuses"][1]["biz_opaque_callback_data"] = "msg-nosso".into();
    assert_eq!(parse(v).message.unwrap().callback_data().as_deref(), Some("msg-nosso"));
}
