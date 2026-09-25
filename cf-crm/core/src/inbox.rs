//! Entrada do webhook do Kapso, já dentro do DO do tenant. Cada evento é
//! processado numa transação: recibo + contato + conversa + mensagem, ou nada.
//!
//! Duas camadas de dedupe, porque o Kapso reentrega de dois jeitos:
//! - mesma entrega repetida (retry): mesma `X-Idempotency-Key` -> `webhook_receipts`;
//! - lote que falhou reenviado item a item, cada um com chave NOVA -> o wamid
//!   (`uq_messages_external`) segura.

use crate::db::{exec, one, Db};
use crate::messaging::{find_or_create_conversation, preview};
use crate::{CoreError, Ctx, Result};
use crm_schema::kapso::{self, EventPayload, WaMessage};
use crm_schema::model::{Contact, ConversationStatus, Direction, MessageStatus, MessageType, SentVia};
use serde::Serialize;
use serde_json::{json, Value};

/// Saída cuja resposta do envio ainda não foi gravada pode ter o status chegando
/// primeiro. Até este prazo o evento volta pra fila; depois a mensagem é adotada
/// como enviada por fora do CRM (API do Kapso, painel, outra integração).
pub const ADOPT_UNKNOWN_OUTBOUND_AFTER_MS: i64 = 2 * 60 * 1000;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Ingested {
    /// Mensagem nova gravada.
    Stored { message_id: String },
    /// Status aplicado numa mensagem que já existia.
    StatusUpdated { message_id: String },
    /// Já processado (mesma entrega ou mesmo wamid).
    Duplicate,
    /// Evento que o CRM não usa.
    Ignored { reason: String },
}

pub fn ingest<D: Db>(ctx: &Ctx<D>, event: &str, idempotency_key: &str, payload: &EventPayload) -> Result<Ingested> {
    if idempotency_key.trim().is_empty() {
        return Err(CoreError::validation("idempotency_key_required", "evento sem chave de idempotência"));
    }
    ctx.db.atomic(|| {
        let fresh = ctx.db.query(
            "INSERT INTO webhook_receipts (idempotency_key, event, received_at) VALUES (?, ?, ?)
             ON CONFLICT (idempotency_key) DO NOTHING RETURNING idempotency_key",
            &[json!(idempotency_key), json!(event), json!(ctx.now_ms)],
        )?;
        if fresh.is_empty() {
            return Ok(Ingested::Duplicate);
        }
        match event {
            kapso::EVENT_MESSAGE_RECEIVED
            | kapso::EVENT_MESSAGE_SENT
            | kapso::EVENT_MESSAGE_DELIVERED
            | kapso::EVENT_MESSAGE_READ
            | kapso::EVENT_MESSAGE_FAILED => match &payload.message {
                Some(m) => message_event(ctx, payload, m),
                None => Ok(Ingested::Ignored { reason: "evento de mensagem sem message".into() }),
            },
            other => Ok(Ingested::Ignored { reason: format!("evento não tratado: {other}") }),
        }
    })
}

fn message_event<D: Db>(ctx: &Ctx<D>, p: &EventPayload, m: &WaMessage) -> Result<Ingested> {
    let meta = m.meta();
    let direction = match meta.direction.as_deref() {
        Some("outbound") => Direction::Outbound,
        Some("inbound") => Direction::Inbound,
        _ if m.rest.contains_key("from") => Direction::Inbound,
        _ => return Ok(Ingested::Ignored { reason: "mensagem sem direção".into() }),
    };
    match direction {
        Direction::Inbound => inbound(ctx, p, m),
        Direction::Outbound => outbound(ctx, p, m),
    }
}

/// `wa_id` do contato: o telefone da conversa sem `+`; na falta, o `from` da Meta.
fn contact_wa_id(p: &EventPayload, m: &WaMessage) -> Option<String> {
    let raw = p
        .conversation
        .as_ref()
        .and_then(|c| c.phone_number.clone())
        .or_else(|| m.rest.get("from").and_then(Value::as_str).map(str::to_string))?;
    let digits: String = raw.chars().filter(char::is_ascii_digit).collect();
    (8..=15).contains(&digits.len()).then_some(digits)
}

/// Acha o contato pelo `wa_id`; senão por telefone (contato cadastrado à mão antes
/// de mandar mensagem), e aí grava o `wa_id` nele; senão cria.
fn contact_for<D: Db>(ctx: &Ctx<D>, wa_id: &str, profile_name: Option<&str>) -> Result<Contact> {
    let by_wa: Option<Contact> = one(
        ctx.db,
        "SELECT * FROM contacts WHERE wa_id = ? AND merged_into_id IS NULL ORDER BY created_at LIMIT 1",
        &[json!(wa_id)],
    )?;
    if let Some(c) = by_wa {
        if profile_name.is_some() && c.display_name.as_deref() != profile_name {
            exec(
                ctx.db,
                "UPDATE contacts SET display_name = ?, updated_at = ? WHERE id = ?",
                &[json!(profile_name), json!(ctx.now_ms), json!(c.id)],
            )?;
        }
        return crate::contacts::get(ctx, &c.id);
    }
    let phone = format!("+{wa_id}");
    let by_phone: Option<Contact> = one(
        ctx.db,
        "SELECT * FROM contacts WHERE phone_e164 = ? AND wa_id IS NULL AND merged_into_id IS NULL ORDER BY created_at LIMIT 1",
        &[json!(phone)],
    )?;
    if let Some(c) = by_phone {
        exec(
            ctx.db,
            "UPDATE contacts SET wa_id = ?, display_name = COALESCE(?, display_name), updated_at = ? WHERE id = ?",
            &[json!(wa_id), json!(profile_name), json!(ctx.now_ms), json!(c.id)],
        )?;
        return crate::contacts::get(ctx, &c.id);
    }
    // Número já usado por outro contato com outro wa_id (nono dígito): não
    // repete o telefone, pra não bater no índice único; o wa_id basta.
    let phone_taken = one::<Value>(
        ctx.db,
        "SELECT 1 AS x FROM contacts WHERE phone_e164 = ? AND merged_into_id IS NULL",
        &[json!(phone)],
    )?
    .is_some();
    let id = ctx.new_id();
    exec(
        ctx.db,
        "INSERT INTO contacts (id, display_name, phone_e164, wa_id, source, created_at, updated_at)
         VALUES (?, ?, ?, ?, 'whatsapp', ?, ?)",
        &[
            json!(id),
            json!(profile_name),
            json!(if phone_taken { None } else { Some(phone) }),
            json!(wa_id),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    crate::contacts::get(ctx, &id)
}

fn message_type(m: &WaMessage) -> MessageType {
    MessageType::parse(&m.kind).unwrap_or(MessageType::Unsupported)
}

struct NewRow<'a> {
    conversation_id: &'a str,
    contact_id: &'a str,
    direction: Direction,
    status: MessageStatus,
    sent_via: SentVia,
    sent_at: i64,
}

/// INSERT ... ON CONFLICT (wamid) DO NOTHING. Devolve o id só se inseriu.
fn insert_message<D: Db>(ctx: &Ctx<D>, m: &WaMessage, row: NewRow) -> Result<Option<String>> {
    let meta = m.meta();
    let kind = message_type(m);
    let media = meta.media_data.clone().unwrap_or_default();
    let mut metadata = serde_json::Map::new();
    if let Some(origin) = &meta.origin {
        metadata.insert("kapso_origin".into(), json!(origin));
    }
    if let Some(url) = &media.url {
        // URL da mídia no Kapso; a cópia pro R2 ainda não existe.
        metadata.insert("kapso_media_url".into(), json!(url));
    }
    if let Some(name) = &media.filename {
        metadata.insert("media_filename".into(), json!(name));
    }
    if let Some(t) = &meta.transcript {
        metadata.insert("transcript".into(), t.clone());
    }
    if kind == MessageType::Unsupported {
        metadata.insert("wa_type".into(), json!(m.kind));
    }
    let (template_name, template_language) = if kind == MessageType::Template {
        let t = m.rest.get("template");
        let name = t.and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("desconhecido");
        let lang = t
            .and_then(|t| t.get("language"))
            .and_then(|l| l.get("code").or(Some(l)))
            .and_then(Value::as_str)
            .unwrap_or("und");
        (Some(name.to_string()), Some(lang.to_string()))
    } else {
        (None, None)
    };
    let err = (row.status == MessageStatus::Failed).then(|| m.last_error()).flatten();
    let at = |s: MessageStatus| (row.status == s).then_some(ctx.now_ms);
    let id = ctx.new_id();
    let inserted = ctx.db.query(
        "INSERT INTO messages (id, conversation_id, contact_id, external_id, direction, type, status, body,
                               template_name, template_language, media_meta_id, media_mime, media_size_bytes,
                               reply_to_external_id, sent_via, error_code, error_message, sent_at,
                               delivered_at, read_at, failed_at, metadata, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (external_id) WHERE external_id IS NOT NULL DO NOTHING
         RETURNING id",
        &[
            json!(id),
            json!(row.conversation_id),
            json!(row.contact_id),
            json!(m.id),
            json!(row.direction.as_str()),
            json!(kind.as_str()),
            json!(row.status.as_str()),
            json!(m.display_text()),
            json!(template_name),
            json!(template_language),
            json!(m.media_id()),
            json!(media.content_type),
            json!(media.byte_size),
            json!(m.reply_to()),
            json!(row.sent_via.as_str()),
            json!(err.as_ref().map(|e| e.code.clone())),
            json!(err.as_ref().map(|e| e.message.clone())),
            json!(row.sent_at),
            json!(at(MessageStatus::Delivered).or(at(MessageStatus::Read))),
            json!(at(MessageStatus::Read)),
            json!(at(MessageStatus::Failed)),
            json!(Value::Object(metadata).to_string()),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    Ok(inserted.first().map(|_| id))
}

fn inbound<D: Db>(ctx: &Ctx<D>, p: &EventPayload, m: &WaMessage) -> Result<Ingested> {
    let Some(wa_id) = contact_wa_id(p, m) else {
        return Ok(Ingested::Ignored { reason: "mensagem sem telefone do contato".into() });
    };
    let profile = p.conversation.as_ref().and_then(|c| c.kapso.as_ref()).and_then(|k| k.contact_name.clone());
    let contact = contact_for(ctx, &wa_id, profile.as_deref())?;
    let kapso_conv = p.conversation.as_ref().and_then(|c| c.id.as_deref());
    let conv = find_or_create_conversation(ctx, &contact.id, &p.phone_number_id, kapso_conv)?;
    let sent_at = m.timestamp_ms().unwrap_or(ctx.now_ms);
    let row = NewRow {
        conversation_id: &conv.id,
        contact_id: &contact.id,
        direction: Direction::Inbound,
        status: MessageStatus::Received,
        sent_via: SentVia::Contact,
        sent_at,
    };
    let Some(message_id) = insert_message(ctx, m, row)? else {
        return Ok(Ingested::Duplicate);
    };
    // Histórico importado não é mensagem nova: não conta como não lida nem reabre.
    let live = m.meta().origin.as_deref() != Some(kapso::ORIGIN_HISTORY_SYNC);
    exec(
        ctx.db,
        "UPDATE conversations SET
            last_inbound_at = MAX(COALESCE(last_inbound_at, 0), ?1),
            last_message_preview = CASE WHEN ?1 >= COALESCE(last_message_at, 0) THEN ?2 ELSE last_message_preview END,
            last_message_at = MAX(COALESCE(last_message_at, 0), ?1),
            unread_count = unread_count + ?3,
            status = CASE WHEN ?4 AND status = 'closed' THEN ?5 ELSE status END,
            status_changed_at = CASE WHEN ?4 AND status = 'closed' THEN ?6 ELSE status_changed_at END,
            updated_at = ?6
         WHERE id = ?7",
        &[
            json!(sent_at),
            json!(preview(m.display_text().as_deref(), message_type(m))),
            json!(i64::from(live)),
            json!(live),
            json!(ConversationStatus::Open.as_str()),
            json!(ctx.now_ms),
            json!(conv.id),
        ],
    )?;
    exec(
        ctx.db,
        "UPDATE contacts SET last_activity_at = MAX(COALESCE(last_activity_at, 0), ?) WHERE id = ?",
        &[json!(sent_at), json!(contact.id)],
    )?;
    Ok(Ingested::Stored { message_id })
}

fn status_of(meta_status: Option<&str>) -> Option<MessageStatus> {
    match meta_status? {
        "sent" => Some(MessageStatus::Sent),
        "delivered" => Some(MessageStatus::Delivered),
        "read" => Some(MessageStatus::Read),
        "failed" => Some(MessageStatus::Failed),
        _ => None,
    }
}

fn outbound<D: Db>(ctx: &Ctx<D>, p: &EventPayload, m: &WaMessage) -> Result<Ingested> {
    let meta = m.meta();
    let status = status_of(meta.status.as_deref());

    // 1. Mensagem nossa já com wamid gravado.
    let mut known: Option<String> = one::<Value>(ctx.db, "SELECT id FROM messages WHERE external_id = ?", &[json!(m.id)])?
        .and_then(|r| r.get("id").and_then(Value::as_str).map(str::to_string));

    // 2. Mensagem nossa cuja resposta do envio ainda não foi gravada: a Meta devolve
    //    no status o id que mandamos em biz_opaque_callback_data.
    if known.is_none() {
        if let Some(our_id) = m.callback_data() {
            let adopted = ctx.db.query(
                "UPDATE messages SET external_id = ?, updated_at = ?
                 WHERE id = ? AND direction = 'outbound' AND external_id IS NULL
                 RETURNING id",
                &[json!(m.id), json!(ctx.now_ms), json!(our_id)],
            )?;
            known = adopted.first().map(|_| our_id);
        }
    }

    let Some(message_id) = known else {
        return unknown_outbound(ctx, p, m, status);
    };
    if let Some(s) = status {
        apply_status(ctx, &message_id, s, m)?;
    }
    Ok(Ingested::StatusUpdated { message_id })
}

/// Saída que o CRM não conhece. Vinda do app do WhatsApp Business ou do histórico,
/// grava na hora. Vinda da API, pode ser nossa com a resposta ainda em trânsito:
/// espera um pouco (a fila tenta de novo) antes de adotar.
fn unknown_outbound<D: Db>(ctx: &Ctx<D>, p: &EventPayload, m: &WaMessage, status: Option<MessageStatus>) -> Result<Ingested> {
    let origin = m.meta().origin;
    let sent_at = m.timestamp_ms().unwrap_or(ctx.now_ms);
    let sent_via = match origin.as_deref() {
        Some(kapso::ORIGIN_BUSINESS_APP) | Some(kapso::ORIGIN_HISTORY_SYNC) => SentVia::ExternalDevice,
        _ => {
            if ctx.now_ms - sent_at < ADOPT_UNKNOWN_OUTBOUND_AFTER_MS {
                return Err(CoreError::retry(
                    "outbound_not_yet_known",
                    "status de mensagem enviada que ainda não tem wamid gravado; tentar de novo",
                ));
            }
            SentVia::Api
        }
    };
    let Some(wa_id) = contact_wa_id(p, m) else {
        return Ok(Ingested::Ignored { reason: "mensagem sem telefone do contato".into() });
    };
    let contact = contact_for(ctx, &wa_id, None)?;
    let kapso_conv = p.conversation.as_ref().and_then(|c| c.id.as_deref());
    let conv = find_or_create_conversation(ctx, &contact.id, &p.phone_number_id, kapso_conv)?;
    let row = NewRow {
        conversation_id: &conv.id,
        contact_id: &contact.id,
        direction: Direction::Outbound,
        status: status.unwrap_or(MessageStatus::Sent),
        sent_via,
        sent_at,
    };
    let Some(message_id) = insert_message(ctx, m, row)? else {
        return Ok(Ingested::Duplicate);
    };
    exec(
        ctx.db,
        "UPDATE conversations SET
            last_outbound_at = MAX(COALESCE(last_outbound_at, 0), ?1),
            last_message_preview = CASE WHEN ?1 >= COALESCE(last_message_at, 0) THEN ?2 ELSE last_message_preview END,
            last_message_at = MAX(COALESCE(last_message_at, 0), ?1),
            updated_at = ?3
         WHERE id = ?4",
        &[
            json!(sent_at),
            json!(preview(m.display_text().as_deref(), message_type(m))),
            json!(ctx.now_ms),
            json!(conv.id),
        ],
    )?;
    Ok(Ingested::Stored { message_id })
}

/// Só pra frente: se o status regride, o trigger descarta o UPDATE inteiro.
pub fn apply_status<D: Db>(ctx: &Ctx<D>, message_id: &str, status: MessageStatus, m: &WaMessage) -> Result<()> {
    let err = (status == MessageStatus::Failed).then(|| m.last_error()).flatten();
    let now = ctx.now_ms;
    exec(
        ctx.db,
        "UPDATE messages SET
            status = ?1,
            delivered_at = CASE WHEN ?1 IN ('delivered', 'read') THEN COALESCE(delivered_at, ?2) ELSE delivered_at END,
            read_at = CASE WHEN ?1 = 'read' THEN COALESCE(read_at, ?2) ELSE read_at END,
            failed_at = CASE WHEN ?1 = 'failed' THEN COALESCE(failed_at, ?2) ELSE failed_at END,
            error_code = CASE WHEN ?1 = 'failed' THEN COALESCE(?3, error_code, 'unknown') ELSE error_code END,
            error_message = CASE WHEN ?1 = 'failed' THEN COALESCE(?4, error_message) ELSE error_message END,
            updated_at = ?2
         WHERE id = ?5",
        &[
            json!(status.as_str()),
            json!(now),
            json!(err.as_ref().map(|e| e.code.clone())),
            json!(err.as_ref().map(|e| e.message.clone())),
            json!(message_id),
        ],
    )
}

/// Recibos com mais de 7 dias: o Kapso não reentrega depois de ~50s.
pub fn purge_receipts<D: Db>(ctx: &Ctx<D>) -> Result<()> {
    exec(
        ctx.db,
        "DELETE FROM webhook_receipts WHERE received_at < ?",
        &[json!(ctx.now_ms - 7 * 24 * 60 * 60 * 1000)],
    )
}
