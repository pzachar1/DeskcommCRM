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
