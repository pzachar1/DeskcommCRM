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
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::Sha256;

pub const HEADER_EVENT: &str = "X-Webhook-Event";
pub const HEADER_SIGNATURE: &str = "X-Webhook-Signature";
pub const HEADER_IDEMPOTENCY_KEY: &str = "X-Idempotency-Key";
pub const HEADER_PAYLOAD_VERSION: &str = "X-Webhook-Payload-Version";
pub const HEADER_BATCH: &str = "X-Webhook-Batch";

pub const EVENT_MESSAGE_RECEIVED: &str = "whatsapp.message.received";
pub const EVENT_MESSAGE_SENT: &str = "whatsapp.message.sent";
pub const EVENT_MESSAGE_DELIVERED: &str = "whatsapp.message.delivered";
pub const EVENT_MESSAGE_READ: &str = "whatsapp.message.read";
pub const EVENT_MESSAGE_FAILED: &str = "whatsapp.message.failed";

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

/// Uma entrega vira uma lista de `(evento, payload)`. Com buffering ligado o corpo é
/// `{ "type", "batch": true, "data": [...] }` mesmo com um evento só, então decide
/// pelo campo `batch`, nunca pela forma. O nome do evento vem do corpo do lote ou,
/// fora de lote, do header `X-Webhook-Event`.
pub fn split_delivery(event_header: Option<&str>, body: Value) -> Vec<(String, Value)> {
    let is_batch = body.get("batch").and_then(Value::as_bool).unwrap_or(false);
    if !is_batch {
        let event = event_header.unwrap_or_default().to_string();
        return vec![(event, body)];
    }
    let event = body
        .get("type")
        .and_then(Value::as_str)
        .or(event_header)
        .unwrap_or_default()
        .to_string();
    match body.get("data") {
        Some(Value::Array(items)) => items.iter().cloned().map(|p| (event.clone(), p)).collect(),
        _ => Vec::new(),
    }
}

/// Payload v2 de um evento `whatsapp.message.*`. Só o que o CRM usa; o resto fica
/// em `message.rest`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventPayload {
    /// Id do número na Meta. É a única coisa que decide o tenant.
    pub phone_number_id: String,
    pub message: Option<WaMessage>,
    pub conversation: Option<WaConversation>,
    #[serde(default)]
    pub is_new_conversation: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WaMessage {
    /// wamid
    pub id: String,
    /// Unix em SEGUNDOS, como string (formato da Meta).
    pub timestamp: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub kapso: Option<KapsoMessageMeta>,
    /// `text`, `image`, `from`, `context`... como vieram.
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct KapsoMessageMeta {
    pub direction: Option<String>,
    pub status: Option<String>,
    pub origin: Option<String>,
    /// Texto que representa a mensagem (legenda, transcrição, descrição da mídia).
    pub content: Option<String>,
    pub media_data: Option<MediaData>,
    pub transcript: Option<Value>,
    /// Histórico cru dos status da Meta, em ordem.
    #[serde(default)]
    pub statuses: Vec<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaData {
    pub url: Option<String>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub byte_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WaConversation {
    pub id: Option<String>,
    /// Telefone do contato em E.164.
    pub phone_number: Option<String>,
    pub kapso: Option<KapsoConversationMeta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct KapsoConversationMeta {
    pub contact_name: Option<String>,
}

/// Erro da Meta anexado a um status `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaError {
    pub code: String,
    pub message: String,
}

impl WaMessage {
    pub fn meta(&self) -> KapsoMessageMeta {
        self.kapso.clone().unwrap_or_default()
    }

    /// Timestamp da Meta em ms. `None` se vier ausente ou fora do formato.
    pub fn timestamp_ms(&self) -> Option<i64> {
        self.timestamp.as_deref()?.trim().parse::<i64>().ok().map(|s| s * 1000)
    }

    /// Texto para mostrar: corpo do texto, legenda da mídia, título do botão,
    /// emoji da reação. Na falta, o `content` que o Kapso monta.
    pub fn display_text(&self) -> Option<String> {
        let block = self.rest.get(&self.kind);
        let pick = |path: &[&str]| -> Option<String> {
            let mut v = block?;
            for k in path {
                v = v.get(*k)?;
            }
            v.as_str().map(str::to_string)
        };
        pick(&["body"])
            .or_else(|| pick(&["caption"]))
            .or_else(|| pick(&["emoji"]))
            .or_else(|| pick(&["text"]))
            .or_else(|| pick(&["button_reply", "title"]))
            .or_else(|| pick(&["list_reply", "title"]))
            .or_else(|| self.kapso.as_ref().and_then(|k| k.content.clone()))
            .filter(|s| !s.trim().is_empty())
    }

    /// Id da mídia na Meta (`image.id`, `audio.id`...).
    pub fn media_id(&self) -> Option<String> {
        self.rest.get(&self.kind)?.get("id")?.as_str().map(str::to_string)
    }

    /// wamid da mensagem respondida, quando é resposta.
    pub fn reply_to(&self) -> Option<String> {
        self.rest.get("context")?.get("id")?.as_str().map(str::to_string)
    }

    /// Primeiro erro do status mais recente que trouxer erro.
    pub fn last_error(&self) -> Option<WaError> {
        let statuses = &self.kapso.as_ref()?.statuses;
        statuses.iter().rev().find_map(|s| {
            let e = s.get("errors")?.as_array()?.first()?;
            let code = match e.get("code") {
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::String(c)) => c.clone(),
                _ => "unknown".into(),
            };
            let message = e
                .get("message")
                .or_else(|| e.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("erro sem descrição")
                .to_string();
            Some(WaError { code, message })
        })
    }

    /// `biz_opaque_callback_data` que mandamos no envio e a Meta devolve nos status.
    /// O CRM põe ali o id da própria mensagem, pra reconhecer o status mesmo se ele
    /// chegar antes de a resposta do envio ter sido gravada.
    pub fn callback_data(&self) -> Option<String> {
        let statuses = &self.kapso.as_ref()?.statuses;
        statuses
            .iter()
            .find_map(|s| s.get("biz_opaque_callback_data")?.as_str().map(str::to_string))
    }
}
