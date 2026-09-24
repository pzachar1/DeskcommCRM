//! Contrato do webhook do Kapso (payload v2).
//!
//! Headers de toda entrega: `X-Webhook-Event`, `X-Webhook-Signature`
//! (HMAC-SHA256 do corpo cru, em hex), `X-Idempotency-Key`,
//! `X-Webhook-Payload-Version: v2`. Entrega em lote traz também
//! `X-Webhook-Batch: true` e `X-Batch-Size`, com os eventos em `data[]`.
//!
//! O Kapso tenta 3 vezes contando a primeira (agora, +10s, +40s), desiste em
//! ~50s e quer 200 em até 10s. Por isso o Worker só verifica, enfileira e responde.

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const HEADER_EVENT: &str = "X-Webhook-Event";
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";
pub const HEADER_IDEMPOTENCY_KEY: &str = "X-Idempotency-Key";
pub const HEADER_PAYLOAD_VERSION: &str = "X-Webhook-Payload-Version";
pub const HEADER_BATCH: &str = "X-Webhook-Batch";

/// `message.kapso.origin`: por onde a mensagem entrou.
pub const ORIGIN_CLOUD_API: &str = "cloud_api";
/// Enviada pela equipe no WhatsApp Business App. Grava com `sent_via = external_device`.
pub const ORIGIN_BUSINESS_APP: &str = "business_app";
/// Importação de histórico. Grava, mas nunca dispara agente nem automação.
pub const ORIGIN_HISTORY_SYNC: &str = "history_sync";

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
