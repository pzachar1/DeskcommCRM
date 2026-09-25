//! WhatsApp pelo Kapso, lado do Worker de entrada:
//! - `POST /webhooks/kapso`: confere a assinatura, descobre o tenant pelo número,
//!   põe na Queue e responde 200. O Kapso quer resposta em até 10s e só tenta 3
//!   vezes em ~50s, então nada de trabalho de negócio aqui.
//! - consumer da Queue: entrega cada evento ao DO do tenant.
//! - `/api/v1/whatsapp-numbers`: liga um `phone_number_id` a um tenant (D1).

use crate::auth::{n, s};
use crate::http::{fail, ok};
use crate::ids::now_ms;
use crate::tenant::INTERNAL_HEADER;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use worker::{console_error, console_warn, Env, MessageBatch, MessageExt, Method, QueueRetryOptionsBuilder, Request, Response, Result};

pub const QUEUE_BINDING: &str = "INBOUND";
pub const EVENTS_PATH: &str = "/internal/kapso/events";
/// Espera antes de reentregar um evento que o DO pediu pra tentar depois.
const RETRY_DELAY_S: u32 = 30;

/// Um evento na fila. `payload` vai como texto JSON: passa pela Queue sem
/// conversão de número nenhuma.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEvent {
    pub tenant_id: String,
    pub event: String,
    /// `X-Idempotency-Key` da entrega + posição no lote.
    pub key: String,
    pub payload: String,
}

async fn tenant_of(db: &worker::D1Database, phone_number_id: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        tenant_id: String,
    }
    let row = db
        .prepare("SELECT tenant_id FROM whatsapp_numbers WHERE phone_number_id = ? AND status = 'active'")
        .bind(&[s(phone_number_id)])?
        .first::<Row>(None)
        .await?;
    Ok(row.map(|r| r.tenant_id))
}

pub async fn webhook(mut req: Request, env: &Env) -> Result<Response> {
    use crm_schema::kapso::{split_delivery, verify_signature, HEADER_EVENT, HEADER_IDEMPOTENCY_KEY, HEADER_SIGNATURE};

    let Ok(secret) = env.secret("KAPSO_WEBHOOK_SECRET").map(|s| s.to_string()) else {
        console_error!("KAPSO_WEBHOOK_SECRET não configurado: webhook recusado");
        return fail(503, "webhook_not_configured", "webhook sem segredo configurado");
    };
    let raw = req.bytes().await?;
    let signature = req.headers().get(HEADER_SIGNATURE)?.unwrap_or_default();
    // Assinatura do corpo CRU, antes de qualquer parse.
    if !verify_signature(secret.as_bytes(), &raw, &signature) {
        return fail(401, "invalid_signature", "assinatura inválida");
    }
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return fail(400, "invalid_json", "corpo não é JSON");
    };
    let delivery_key = match req.headers().get(HEADER_IDEMPOTENCY_KEY)? {
        Some(k) if !k.trim().is_empty() => k,
        // Sem chave não há dedupe por entrega; o wamid ainda segura mensagem repetida.
        _ => crate::ids::sha256_hex(&String::from_utf8_lossy(&raw)),
    };
    let header_event = req.headers().get(HEADER_EVENT)?;

    let db = env.d1("DB")?;
    let mut tenants: HashMap<String, Option<String>> = HashMap::new();
    let mut out = Vec::new();
    for (i, (event, payload)) in split_delivery(header_event.as_deref(), body).into_iter().enumerate() {
        let Some(pnid) = payload.get("phone_number_id").and_then(Value::as_str).map(str::to_string) else {
            console_warn!("evento {event} sem phone_number_id: descartado");
            continue;
        };
        if !tenants.contains_key(&pnid) {
            let t = tenant_of(&db, &pnid).await?;
            tenants.insert(pnid.clone(), t);
        }
        let Some(tenant_id) = tenants[&pnid].clone() else {
            // Número não cadastrado. Responder erro faria o Kapso repetir e,
            // depois de muitas falhas, pausar o webhook de TODOS os números.
            console_warn!("phone_number_id {pnid} sem tenant: evento {event} descartado");
            continue;
        };
        out.push(InboundEvent { tenant_id, event, key: format!("{delivery_key}:{i}"), payload: payload.to_string() });
    }

    let queued = out.len();
    let queue = env.queue(QUEUE_BINDING)?;
    // Limite da Queue: 100 mensagens e 256 KB por envio.
    for chunk in out.chunks(25) {
        // Falhou aqui: 500, e o Kapso reentrega. O dedupe no DO segura o que já tinha entrado.
        queue.send_batch(chunk.to_vec()).await?;
    }
    ok(&json!({ "queued": queued }), 200)
}

enum Delivery {
    Done,
    Later,
    Drop(String),
}

async fn deliver(env: &Env, ev: &InboundEvent) -> Result<Delivery> {
    let payload: Value = match serde_json::from_str(&ev.payload) {
        Ok(v) => v,
        Err(e) => return Ok(Delivery::Drop(format!("payload ilegível: {e}"))),
    };
    let body = json!({ "event": ev.event, "key": ev.key, "payload": payload }).to_string().into_bytes();
    let rid = crate::ids::uuid_v7(now_ms());
    let mut resp = crate::to_tenant(
        env,
        &ev.tenant_id,
        None,
        &rid,
        Method::Post,
        EVENTS_PATH,
        Some(body),
        &[(INTERNAL_HEADER, "queue".to_string())],
    )
    .await?;
    let status = resp.status_code();
    Ok(match status {
        200..=299 => Delivery::Done,
        503 => Delivery::Later,
        400..=499 => Delivery::Drop(format!("DO recusou ({status}): {}", resp.text().await.unwrap_or_default())),
        _ => Delivery::Later,
    })
}

/// Consumer da Queue. Cada mensagem é acked ou reagendada sozinha: um evento
/// ruim não segura os outros do lote.
pub async fn consume(batch: MessageBatch<InboundEvent>, env: Env) -> Result<()> {
    let later = QueueRetryOptionsBuilder::new().with_delay_seconds(RETRY_DELAY_S).build();
    for msg in batch.messages()? {
        let ev = msg.body();
        match deliver(&env, ev).await {
            Ok(Delivery::Done) => msg.ack(),
            Ok(Delivery::Later) => msg.retry_with_options(&later),
            Ok(Delivery::Drop(reason)) => {
                console_error!("evento {} ({}) descartado: {reason}", ev.key, ev.event);
                msg.ack();
            }
            Err(e) => {
                console_error!("evento {} ({}) falhou: {e}", ev.key, ev.event);
                msg.retry_with_options(&later);
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ números

#[derive(Debug, Serialize, Deserialize)]
pub struct WhatsappNumber {
    pub phone_number_id: String,
    pub display_phone: String,
    pub waba_id: Option<String>,
    pub label: Option<String>,
    pub status: String,
    pub created_at: i64,
}

#[derive(Deserialize)]
struct NewNumber {
    phone_number_id: String,
    display_phone: String,
    waba_id: Option<String>,
    label: Option<String>,
}

pub async fn list_numbers(env: &Env, tenant_id: &str) -> Result<Response> {
    let rows = env
        .d1("DB")?
        .prepare(
            "SELECT phone_number_id, display_phone, waba_id, label, status, created_at
             FROM whatsapp_numbers WHERE tenant_id = ? ORDER BY created_at",
        )
        .bind(&[s(tenant_id)])?
        .all()
        .await?
        .results::<WhatsappNumber>()?;
    ok(&rows, 200)
}

pub async fn add_number(mut req: Request, env: &Env, tenant_id: &str) -> Result<Response> {
    let input: NewNumber = match crate::http::json_body(&mut req).await {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let pnid = input.phone_number_id.trim();
    if pnid.is_empty() || pnid.len() > 32 || !pnid.bytes().all(|c| c.is_ascii_digit()) {
        return fail(422, "invalid_phone_number_id", "phone_number_id é o id numérico do número na Meta");
    }
    let display = match crm_core::contacts::normalize_phone(&input.display_phone) {
        Ok(p) => p,
        Err(e) => return crate::http::from_core(&e),
    };
    let now = now_ms();
    let clean = |v: Option<String>| v.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
    let (waba_id, label) = (clean(input.waba_id), clean(input.label));
    let db = env.d1("DB")?;
    let result = db
        .prepare(
            "INSERT INTO whatsapp_numbers (phone_number_id, tenant_id, display_phone, waba_id, label, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&[
            s(pnid),
            s(tenant_id),
            s(&display),
            waba_id.as_deref().map(s).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            label.as_deref().map(s).unwrap_or(worker::wasm_bindgen::JsValue::NULL),
            n(now),
            n(now),
        ])?
        .run()
        .await;
    if let Err(e) = result {
        if e.to_string().contains("UNIQUE constraint failed") {
            // Não diz de quem é: não vaza a existência de outro tenant.
            return fail(409, "phone_number_taken", "este número já está cadastrado");
        }
        return Err(e);
    }
    ok(
        &WhatsappNumber {
            phone_number_id: pnid.to_string(),
            display_phone: display,
            waba_id,
            label,
            status: "active".into(),
            created_at: now,
        },
        201,
    )
}

/// O número é deste tenant? Quem abre conversa escolhe o número, e o DO não
/// enxerga o D1.
pub async fn number_belongs(env: &Env, tenant_id: &str, phone_number_id: &str) -> Result<bool> {
    Ok(tenant_of(&env.d1("DB")?, phone_number_id).await?.as_deref() == Some(tenant_id))
}
