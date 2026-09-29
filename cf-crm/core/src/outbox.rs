//! Drenagem da outbox, sem I/O: o Alarm do DO pede os envios devidos
//! ([`claim_due`]), chama o Kapso e devolve o resultado ([`complete`]).
//!
//! Garantia: uma mensagem sai no máximo uma vez. Cada envio é marcado
//! `in_flight_at` ANTES da chamada. Se a resposta nunca voltar (o DO caiu no
//! meio), a mensagem vira `failed` com `send_outcome_unknown` em vez de sair de
//! novo. Mensagem repetida pro cliente é pior que uma falha que a equipe vê e reenvia.

use crate::db::{all, exec, one, Db};
use crate::messaging::{get_message, recipient};
use crate::{contacts, CoreError, Ctx, Result};
use crm_schema::model::{Message, MessageStatus, MessageType, OutboxItem, OutboxKind};
use serde_json::{json, Value};

/// Tentativas antes de desistir de um erro temporário (429, 5xx, rede).
pub const MAX_ATTEMPTS: i64 = 6;
/// Quanto tempo esperar a resposta de um envio antes de declarar o resultado desconhecido.
pub const IN_FLIGHT_LEASE_MS: i64 = 5 * 60 * 1000;
const BASE_BACKOFF_MS: i64 = 30 * 1000;
const MAX_BACKOFF_MS: i64 = 30 * 60 * 1000;

/// Um envio pronto pra sair: rota e corpo exatos do `POST /{phone_number_id}/messages`.
#[derive(Debug, Clone)]
pub struct SendJob {
    pub outbox_id: String,
    pub message_id: String,
    pub phone_number_id: String,
    pub body: Value,
}

/// O que a chamada ao Kapso devolveu, já classificado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Accepted { wamid: String },
    Rejected { code: String, message: String, retryable: bool },
}

/// Espera entre tentativas: 30s, 1min, 2min... até 30min.
pub fn backoff_ms(attempts: i64) -> i64 {
    let exp = (attempts - 1).clamp(0, 16) as u32;
    (BASE_BACKOFF_MS.saturating_mul(1 << exp)).min(MAX_BACKOFF_MS)
}

/// Corpo no formato da Cloud API. `biz_opaque_callback_data` leva o id da nossa
/// mensagem: a Meta devolve nos status, e o webhook reconhece a mensagem mesmo se
/// chegar antes de a resposta do envio ser gravada.
pub fn request_body(message: &Message, to: &str) -> Result<Value> {
    let mut body = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "biz_opaque_callback_data": message.id,
    });
    match message.kind {
        MessageType::Text => {
            body["type"] = json!("text");
            body["text"] = json!({ "body": message.body.clone().unwrap_or_default(), "preview_url": false });
        }
        MessageType::Template => {
            let mut template = json!({
                "name": message.template_name,
                "language": { "code": message.template_language },
            });
            if let Some(components) = &message.template_params {
                template["components"] = components.clone();
            }
            body["type"] = json!("template");
            body["template"] = template;
        }
        other => {
            return Err(CoreError::validation(
                "unsupported_outbound_type",
                format!("envio de {} ainda não é suportado", other.as_str()),
            ))
        }
    }
    if let Some(reply_to) = &message.reply_to_external_id {
        body["context"] = json!({ "message_id": reply_to });
    }
    Ok(body)
}

/// Erros da Meta que passam sozinhos: limite de taxa e indisponibilidade.
fn retryable_meta_code(code: &str) -> bool {
    matches!(code, "4" | "80007" | "130429" | "131000" | "131016" | "131048" | "131056" | "133004")
}

/// Classifica a resposta HTTP. `status = 0` é falha de rede (sem resposta).
pub fn classify(status: u16, body: &Value) -> SendOutcome {
    if (200..300).contains(&status) {
        let wamid = body
            .get("messages")
            .and_then(|m| m.get(0))
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str);
        return match wamid {
            Some(id) if !id.is_empty() => SendOutcome::Accepted { wamid: id.to_string() },
            _ => SendOutcome::Rejected {
                code: "invalid_response".into(),
                message: "o Kapso respondeu sem o id da mensagem".into(),
                // Pode ter saído. Não arriscar mandar de novo.
                retryable: false,
            },
        };
    }
    let err = body.get("error");
    let meta_code = err.and_then(|e| e.get("code")).map(|c| match c {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let message = err
        .and_then(|e| e.get("message").or_else(|| e.get("error_user_msg")))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| if status == 0 { "sem resposta do Kapso".into() } else { format!("Kapso respondeu HTTP {status}") });
    let retryable =
        status == 0 || status == 408 || status == 429 || status >= 500 || meta_code.as_deref().is_some_and(retryable_meta_code);
    let code = meta_code.unwrap_or_else(|| if status == 0 { "network_error".into() } else { format!("http_{status}") });
    SendOutcome::Rejected { code, message, retryable }
}

fn fail_message<D: Db>(ctx: &Ctx<D>, message_id: &str, code: &str, message: &str) -> Result<()> {
    exec(
        ctx.db,
        "UPDATE messages SET status = ?, failed_at = ?, error_code = ?, error_message = ?, updated_at = ? WHERE id = ?",
        &[
            json!(MessageStatus::Failed.as_str()),
            json!(ctx.now_ms),
            json!(code),
            json!(message),
            json!(ctx.now_ms),
            json!(message_id),
        ],
    )
}

fn delete_item<D: Db>(ctx: &Ctx<D>, outbox_id: &str) -> Result<()> {
    exec(ctx.db, "DELETE FROM outbox WHERE id = ?", &[json!(outbox_id)])
}

/// Resolve envios cuja resposta nunca voltou e marca até `limit` envios devidos
/// como em andamento. O que voltar aqui PRECISA terminar em [`complete`].
pub fn claim_due<D: Db>(ctx: &Ctx<D>, limit: u32) -> Result<Vec<SendJob>> {
    ctx.db.atomic(|| {
        let stale: Vec<OutboxItem> = all(
            ctx.db,
            "SELECT * FROM outbox WHERE kind = ? AND in_flight_at IS NOT NULL AND in_flight_at <= ?",
            &[json!(OutboxKind::SendMessage.as_str()), json!(ctx.now_ms - IN_FLIGHT_LEASE_MS)],
        )?;
        for item in stale {
            let msg = get_message(ctx, &item.ref_id)?;
            // Se o webhook já trouxe o wamid (callback data), saiu: nada a fazer.
            if msg.external_id.is_none() {
                fail_message(
                    ctx,
                    &msg.id,
                    "send_outcome_unknown",
                    "o envio não respondeu a tempo e pode ou não ter saído; confira com o contato antes de reenviar",
                )?;
            }
            delete_item(ctx, &item.id)?;
        }

        let due: Vec<OutboxItem> = all(
            ctx.db,
            "SELECT * FROM outbox WHERE kind = ? AND in_flight_at IS NULL AND next_attempt_at <= ?
             ORDER BY next_attempt_at, created_at LIMIT ?",
            &[json!(OutboxKind::SendMessage.as_str()), json!(ctx.now_ms), json!(limit.clamp(1, 100))],
        )?;
        let mut jobs = Vec::with_capacity(due.len());
        for item in due {
            let msg = get_message(ctx, &item.ref_id)?;
            if msg.status != MessageStatus::Queued {
                // Já resolvida por outro caminho (webhook com callback data).
                delete_item(ctx, &item.id)?;
                continue;
            }
            let conv = crate::messaging::get_conversation(ctx, &msg.conversation_id)?;
            let contact = contacts::get(ctx, &msg.contact_id)?;
            let prepared = recipient(&contact)
                .ok_or_else(|| CoreError::validation("contact_without_phone", "o contato não tem telefone nem WhatsApp"))
                .and_then(|to| request_body(&msg, &to));
            let body = match prepared {
                Ok(b) => b,
                Err(e) => {
                    fail_message(ctx, &msg.id, e.code(), &e.to_string())?;
                    delete_item(ctx, &item.id)?;
                    continue;
                }
            };
            exec(
                ctx.db,
                "UPDATE outbox SET in_flight_at = ?, attempts = attempts + 1 WHERE id = ?",
                &[json!(ctx.now_ms), json!(item.id)],
            )?;
            jobs.push(SendJob { outbox_id: item.id, message_id: msg.id, phone_number_id: conv.phone_number_id, body });
        }
        Ok(jobs)
    })
}

/// Grava o resultado de um envio marcado por [`claim_due`].
pub fn complete<D: Db>(ctx: &Ctx<D>, job: &SendJob, outcome: &SendOutcome) -> Result<()> {
    ctx.db.atomic(|| {
        let Some(item) = one::<OutboxItem>(ctx.db, "SELECT * FROM outbox WHERE id = ?", &[json!(job.outbox_id)])? else {
            // Já resolvido (lease vencido tratado por outra rodada).
            return Ok(());
        };
        match outcome {
            SendOutcome::Accepted { wamid } => {
                // O webhook pode ter chegado antes e, sem callback data, adotado a
                // mensagem como de outra origem. Ela é esta: tira a cópia e herda o status.
                let copy: Option<Message> = one(
                    ctx.db,
                    "SELECT * FROM messages WHERE external_id = ? AND id <> ?",
                    &[json!(wamid), json!(job.message_id)],
                )?;
                if let Some(c) = &copy {
                    exec(ctx.db, "DELETE FROM messages WHERE id = ?", &[json!(c.id)])?;
                }
                exec(
                    ctx.db,
                    "UPDATE messages SET external_id = ?, status = ?, accepted_at = ?, updated_at = ? WHERE id = ?",
                    &[
                        json!(wamid),
                        json!(MessageStatus::Accepted.as_str()),
                        json!(ctx.now_ms),
                        json!(ctx.now_ms),
                        json!(job.message_id),
                    ],
                )?;
                // Status que já tinha chegado pelo webhook: se era 'sent', o UPDATE
                // acima regrediria e o trigger o descartou inteiro; garante o wamid.
                exec(
                    ctx.db,
                    "UPDATE messages SET external_id = ? WHERE id = ? AND external_id IS NULL",
                    &[json!(wamid), json!(job.message_id)],
                )?;
                if let Some(c) = copy {
                    exec(
                        ctx.db,
                        "UPDATE messages SET status = ?, delivered_at = COALESCE(delivered_at, ?), read_at = COALESCE(read_at, ?),
                                failed_at = COALESCE(failed_at, ?), error_code = COALESCE(error_code, ?),
                                error_message = COALESCE(error_message, ?), updated_at = ?
                         WHERE id = ?",
                        &[
                            json!(c.status.as_str()),
                            json!(c.delivered_at),
                            json!(c.read_at),
                            json!(c.failed_at),
                            json!(c.error_code),
                            json!(c.error_message),
                            json!(ctx.now_ms),
                            json!(job.message_id),
                        ],
                    )?;
                }
                delete_item(ctx, &item.id)
            }
            SendOutcome::Rejected { code, message, retryable } => {
                if *retryable && item.attempts < MAX_ATTEMPTS {
                    exec(
                        ctx.db,
                        "UPDATE outbox SET in_flight_at = NULL, next_attempt_at = ?, last_error = ? WHERE id = ?",
                        &[json!(ctx.now_ms + backoff_ms(item.attempts)), json!(format!("{code}: {message}")), json!(item.id)],
                    )
                } else {
                    fail_message(ctx, &job.message_id, code, message)?;
                    delete_item(ctx, &item.id)
                }
            }
        }
    })
}

/// Quando o Alarm precisa rodar de novo: próximo envio devido, ou o vencimento
/// do prazo de um envio em andamento. `None` = outbox vazia.
pub fn next_wake_at<D: Db>(ctx: &Ctx<D>) -> Result<Option<i64>> {
    let row: Option<Value> = one(
        ctx.db,
        "SELECT MIN(CASE WHEN in_flight_at IS NULL THEN next_attempt_at ELSE in_flight_at + ? END) AS at FROM outbox",
        &[json!(IN_FLIGHT_LEASE_MS)],
    )?;
    Ok(row.and_then(|r| r.get("at").and_then(Value::as_i64)))
}
