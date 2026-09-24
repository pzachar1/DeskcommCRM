//! Contrato do webhook do Kapso (payload v2).
//!
//! Headers de toda entrega: `X-Webhook-Event`, `X-Webhook-Signature`
//! (HMAC-SHA256 do corpo cru, em hex), `X-Idempotency-Key`,
//! `X-Webhook-Payload-Version: v2`. Entrega em lote traz também
//! `X-Webhook-Batch: true` e `X-Batch-Size`, com os eventos em `data[]`.
//!
//! O Kapso tenta 3 vezes (10s, 40s, 90s) e desiste em ~2,5 min, com timeout
//! de 30s (45s em lote). Por isso o Worker só verifica, enfileira e responde 200.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const HEADER_EVENT: &str = "X-Webhook-Event";
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";
pub const HEADER_IDEMPOTENCY_KEY: &str = "X-Idempotency-Key";
pub const HEADER_PAYLOAD_VERSION: &str = "X-Webhook-Payload-Version";
pub const HEADER_BATCH: &str = "X-Webhook-Batch";

/// Confere a assinatura contra o corpo EXATO recebido (antes de qualquer parse).
/// A comparação é em tempo constante (`verify_slice`).
pub fn verify_signature(secret: &[u8], raw_body: &[u8], signature_hex: &str) -> bool {
    let Ok(expected) = hex::decode(signature_hex.trim()) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC aceita chave de qualquer tamanho");
    mac.update(raw_body);
    mac.verify_slice(&expected).is_ok()
}
