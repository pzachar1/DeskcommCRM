//! Conversas e envio. Enviar aqui só GRAVA: a mensagem entra como `queued` junto
//! com uma linha na `outbox`, na mesma transação. Quem fala com o Kapso é o
//! [`crate::outbox`], chamado pelo Alarm do DO.

use crate::db::{all, exec, one, Db};
use crate::{contacts, CoreError, Ctx, Page, Result};
use crm_schema::model::{Contact, Conversation, ConversationStatus, Direction, Message, MessageStatus, MessageType, OutboxKind, SentVia};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Tamanho máximo de texto aceito pela Meta.
pub const MAX_TEXT_CHARS: usize = 4096;
const PREVIEW_CHARS: usize = 120;

/// Linha da caixa de entrada: a conversa e o mínimo do contato pra exibir.
#[derive(Debug, Serialize, Deserialize)]
pub struct InboxItem {
    #[serde(flatten)]
    pub conversation: Conversation,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub service_window_open: bool,
}

#[derive(Debug, Deserialize)]
pub struct OpenConversation {
    pub contact_id: String,
    pub phone_number_id: String,
}

/// Corpo de `POST /conversations/{id}/messages`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Text {
        body: String,
    },
    /// Fora da janela de 24h, só template aprovado na Meta. `components` segue o
    /// formato da Cloud API e vai como está.
    Template {
        name: String,
        language: String,
        #[serde(default)]
        components: Option<Value>,
    },
}

pub fn preview(text: Option<&str>, kind: MessageType) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.chars().take(PREVIEW_CHARS).collect(),
        None => format!("[{}]", kind.as_str()),
    }
}

pub fn get_conversation<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Conversation> {
    one(ctx.db, "SELECT * FROM conversations WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("conversa"))
}

pub fn get_message<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Message> {
    one(ctx.db, "SELECT * FROM messages WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("mensagem"))
}

/// A conversa de um contato num número. Uma só por par (UNIQUE no banco): a
/// sessão de 24h do Kapso vai e vem, a conversa no CRM continua a mesma.
pub fn find_or_create_conversation<D: Db>(
    ctx: &Ctx<D>,
    contact_id: &str,
    phone_number_id: &str,
    kapso_conversation_id: Option<&str>,
) -> Result<Conversation> {
    let found: Option<Conversation> = one(
        ctx.db,
        "SELECT * FROM conversations WHERE contact_id = ? AND phone_number_id = ?",
        &[json!(contact_id), json!(phone_number_id)],
    )?;
    // O id da conversa no Kapso é só ponteiro pra sessão mais recente. Se outra
    // conversa nossa ainda aponta pra ele (contato que trocou de wa_id), solta lá.
    let release = |keep: Option<&str>| {
        exec(
            ctx.db,
            "UPDATE conversations SET kapso_conversation_id = NULL WHERE kapso_conversation_id = ? AND id IS NOT ?",
            &[json!(kapso_conversation_id), json!(keep)],
        )
    };
    if let Some(c) = found {
        // O Kapso abre uma conversa nova a cada sessão; guardamos a mais recente.
        if kapso_conversation_id.is_some() && c.kapso_conversation_id.as_deref() != kapso_conversation_id {
            release(Some(&c.id))?;
            exec(
                ctx.db,
                "UPDATE conversations SET kapso_conversation_id = ?, updated_at = ? WHERE id = ?",
                &[json!(kapso_conversation_id), json!(ctx.now_ms), json!(c.id)],
            )?;
        }
        return get_conversation(ctx, &c.id);
    }
    if kapso_conversation_id.is_some() {
        release(None)?;
    }
    let id = ctx.new_id();
    exec(
        ctx.db,
        "INSERT INTO conversations (id, contact_id, phone_number_id, kapso_conversation_id, status,
                                    status_changed_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            json!(id),
            json!(contact_id),
            json!(phone_number_id),
            json!(kapso_conversation_id),
            json!(ConversationStatus::Open.as_str()),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    get_conversation(ctx, &id)
}

/// Abre (ou devolve) a conversa pra falar primeiro com um contato. Quem garante
/// que o número é deste tenant é o Worker, que conhece o D1.
pub fn open<D: Db>(ctx: &Ctx<D>, input: OpenConversation) -> Result<Conversation> {
    ctx.db.atomic(|| {
        let contact = contacts::get(ctx, &input.contact_id)?;
        ensure_reachable(&contact)?;
        if recipient(&contact).is_none() {
            return Err(CoreError::validation("contact_without_phone", "o contato não tem telefone nem WhatsApp"));
        }
        find_or_create_conversation(ctx, &contact.id, input.phone_number_id.trim(), None)
    })
}

/// Mais recente primeiro. Cursor `last_message_at.id`.
pub fn list_conversations<D: Db>(ctx: &Ctx<D>, status: Option<&str>, limit: u32, cursor: Option<&str>) -> Result<Page<InboxItem>> {
    let limit = limit.clamp(1, 100);
    if let Some(s) = status {
        if ConversationStatus::parse(s).is_none() {
            return Err(CoreError::validation("invalid_status", format!("status desconhecido: {s}")));
        }
    }
    let (c_at, c_id) = match cursor {
        Some(c) => {
            let (t, id) = c.split_once('.').ok_or_else(|| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            let t: i64 = t.parse().map_err(|_| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            (json!(t), json!(id))
        }
        None => (Value::Null, Value::Null),
    };
    let rows: Vec<Value> = ctx
        .db
        .query(
            "SELECT cv.*, COALESCE(ct.name, ct.display_name) AS contact_name, ct.phone_e164 AS contact_phone,
                    COALESCE(cv.last_message_at, cv.created_at) AS sort_at
             FROM conversations cv JOIN contacts ct ON ct.id = cv.contact_id
             WHERE (?1 IS NULL OR cv.status = ?1)
               AND (?2 IS NULL OR (COALESCE(cv.last_message_at, cv.created_at), cv.id) < (?2, ?3))
             ORDER BY sort_at DESC, cv.id DESC
             LIMIT ?4",
            &[json!(status), c_at, c_id, json!(limit + 1)],
        )?
        .into_iter()
        .map(Value::Object)
        .collect();
    let mut items = Vec::with_capacity(rows.len());
    let mut sort_keys = Vec::with_capacity(rows.len());
    for mut row in rows {
        let sort_at = row.get("sort_at").and_then(Value::as_i64).unwrap_or_default();
        let last_inbound = row.get("last_inbound_at").and_then(Value::as_i64);
        if let Some(obj) = row.as_object_mut() {
            obj.remove("sort_at");
            obj.insert("service_window_open".into(), json!(crm_schema::service_window_open(last_inbound, ctx.now_ms)));
        }
        let item: InboxItem = serde_json::from_value(row).map_err(|e| CoreError::Db(format!("linha fora do formato: {e}")))?;
        sort_keys.push(sort_at);
        items.push(item);
    }
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|i| format!("{}.{}", sort_keys[limit as usize - 1], i.conversation.id))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

/// Mais recente primeiro, pela hora da Meta (`sent_at`): mensagem que chegou
/// atrasada pelo webhook cai no lugar certo. Cursor `sent_at.id`.
pub fn list_messages<D: Db>(ctx: &Ctx<D>, conversation_id: &str, limit: u32, cursor: Option<&str>) -> Result<Page<Message>> {
    get_conversation(ctx, conversation_id)?;
    let limit = limit.clamp(1, 200);
    let (c_at, c_id) = match cursor {
        Some(c) => {
            let (t, id) = c.split_once('.').ok_or_else(|| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            let t: i64 = t.parse().map_err(|_| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            (json!(t), json!(id))
        }
        None => (Value::Null, Value::Null),
    };
    let mut items: Vec<Message> = all(
        ctx.db,
        "SELECT * FROM messages
         WHERE conversation_id = ?1 AND (?2 IS NULL OR (sent_at, id) < (?2, ?3))
         ORDER BY sent_at DESC, id DESC
         LIMIT ?4",
        &[json!(conversation_id), c_at, c_id, json!(limit + 1)],
    )?;
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|m| format!("{}.{}", m.sent_at, m.id))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

pub fn mark_read<D: Db>(ctx: &Ctx<D>, conversation_id: &str) -> Result<Conversation> {
    get_conversation(ctx, conversation_id)?;
    exec(
        ctx.db,
        "UPDATE conversations SET unread_count = 0, updated_at = ? WHERE id = ?",
        &[json!(ctx.now_ms), json!(conversation_id)],
    )?;
    get_conversation(ctx, conversation_id)
}

/// Pra quem a Meta entrega: o `wa_id` que ela mesma mandou, senão o telefone.
pub fn recipient(contact: &Contact) -> Option<String> {
    contact
        .wa_id
        .clone()
        .or_else(|| contact.phone_e164.as_deref().map(|p| p.trim_start_matches('+').to_string()))
}

fn ensure_reachable(contact: &Contact) -> Result<()> {
    if contact.is_anonymized {
        return Err(CoreError::conflict("contact_anonymized", "contato anonimizado não recebe mensagem"));
    }
    if contact.is_blocked {
        return Err(CoreError::conflict("contact_blocked", "contato bloqueado (pediu pra sair ou foi bloqueado)"));
    }
    if contact.merged_into_id.is_some() {
        return Err(CoreError::conflict("contact_merged", "contato foi mesclado em outro"));
    }
    Ok(())
}

/// Resultado de um envio. `created = false` quando a `Idempotency-Key` já existia:
/// a mesma mensagem volta, e nada sai de novo.
#[derive(Debug)]
pub struct Sent {
    pub message: Message,
    pub created: bool,
}

pub fn send<D: Db>(ctx: &Ctx<D>, conversation_id: &str, out: Outgoing, idempotency_key: Option<&str>) -> Result<Sent> {
    let key = idempotency_key.map(str::trim).filter(|k| !k.is_empty());
    if let Some(k) = key {
        if k.len() > 200 {
            return Err(CoreError::validation("invalid_idempotency_key", "Idempotency-Key com mais de 200 caracteres"));
        }
    }
    let (kind, body, template_name, template_language, template_params) = match out {
        Outgoing::Text { body } => {
            let body = body.trim().to_string();
            if body.is_empty() {
                return Err(CoreError::validation("empty_message", "mensagem vazia"));
            }
            if body.chars().count() > MAX_TEXT_CHARS {
                return Err(CoreError::validation("message_too_long", format!("texto passa de {MAX_TEXT_CHARS} caracteres")));
            }
            (MessageType::Text, Some(body), None, None, None)
        }
        Outgoing::Template { name, language, components } => {
            let name = name.trim().to_string();
            let language = language.trim().to_string();
            let valid_name = !name.is_empty()
                && name.len() <= 512
                && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_');
            if !valid_name {
                return Err(CoreError::validation("invalid_template", "nome de template: a-z, 0-9 e _"));
            }
            if language.is_empty() || language.len() > 15 {
                return Err(CoreError::validation("invalid_template", "idioma do template obrigatório, ex.: pt_BR"));
            }
            if let Some(c) = &components {
                if !c.is_array() {
                    return Err(CoreError::validation("invalid_template", "components precisa ser uma lista"));
                }
            }
            (MessageType::Template, None, Some(name), Some(language), components)
        }
    };

    ctx.db.atomic(|| {
        if let Some(k) = key {
            let existing: Option<Message> = one(ctx.db, "SELECT * FROM messages WHERE idempotency_key = ?", &[json!(k)])?;
            if let Some(m) = existing {
                if m.conversation_id != conversation_id {
                    return Err(CoreError::conflict("idempotency_key_reused", "Idempotency-Key já usada em outra conversa"));
                }
                return Ok(Sent { message: m, created: false });
            }
        }
        let conv = get_conversation(ctx, conversation_id)?;
        let contact = contacts::get(ctx, &conv.contact_id)?;
        ensure_reachable(&contact)?;
        if recipient(&contact).is_none() {
            return Err(CoreError::validation("contact_without_phone", "o contato não tem telefone nem WhatsApp"));
        }
        if kind == MessageType::Text && !conv.service_window_open(ctx.now_ms) {
            return Err(CoreError::validation(
                "service_window_closed",
                "passaram 24h desde a última mensagem do contato: só dá pra enviar template aprovado",
            ));
        }

        let id = ctx.new_id();
        let sent_via = if ctx.actor.is_some() { SentVia::Crm } else { SentVia::System };
        exec(
            ctx.db,
            "INSERT INTO messages (id, conversation_id, contact_id, idempotency_key, direction, type, status, body,
                                   template_name, template_language, template_params, sent_via, sent_by_user_id,
                                   sent_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                json!(id),
                json!(conv.id),
                json!(contact.id),
                json!(key),
                json!(Direction::Outbound.as_str()),
                json!(kind.as_str()),
                json!(MessageStatus::Queued.as_str()),
                json!(body),
                json!(template_name),
                json!(template_language),
                json!(template_params.as_ref().map(|v| v.to_string())),
                json!(sent_via.as_str()),
                json!(ctx.actor),
                json!(ctx.now_ms),
                json!(ctx.now_ms),
                json!(ctx.now_ms),
            ],
        )?;
        exec(
            ctx.db,
            "INSERT INTO outbox (id, kind, ref_id, attempts, next_attempt_at, created_at) VALUES (?, ?, ?, 0, ?, ?)",
            &[json!(ctx.new_id()), json!(OutboxKind::SendMessage.as_str()), json!(id), json!(ctx.now_ms), json!(ctx.now_ms)],
        )?;
        let shown = body.as_deref().or(template_name.as_deref());
        exec(
            ctx.db,
            "UPDATE conversations SET last_outbound_at = ?1, last_message_at = MAX(COALESCE(last_message_at, 0), ?1),
                    last_message_preview = ?2, updated_at = ?1
             WHERE id = ?3",
            &[json!(ctx.now_ms), json!(preview(shown, kind)), json!(conv.id)],
        )?;
        Ok(Sent { message: get_message(ctx, &id)?, created: true })
    })
}
