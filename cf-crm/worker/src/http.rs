//! Envelope da API: `{ data }` no sucesso, `{ error: { code, message } }` no erro,
//! `X-Request-Id` em toda resposta.

use serde::Serialize;
use serde_json::json;
use worker::{Request, Response, Result};

pub const REQUEST_ID: &str = "X-Request-Id";

pub fn ok<T: Serialize>(data: &T, status: u16) -> Result<Response> {
    Ok(Response::from_json(&json!({ "data": data }))?.with_status(status))
}

pub fn fail(status: u16, code: &str, message: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "error": { "code": code, "message": message } }))?.with_status(status))
}

pub fn from_core(e: &crm_core::CoreError) -> Result<Response> {
    // erro de banco não vaza detalhe interno para o cliente
    let message = match e {
        crm_core::CoreError::Db(_) => "erro interno".to_string(),
        other => other.to_string(),
    };
    fail(e.status(), e.code(), &message)
}

pub fn with_request_id(mut resp: Response, id: &str) -> Result<Response> {
    resp.headers_mut().set(REQUEST_ID, id)?;
    Ok(resp)
}

/// Aceita o id do cliente só se for curto e seguro para log; senão gera um.
pub fn request_id(req: &Request, fresh: impl FnOnce() -> String) -> String {
    req.headers()
        .get(REQUEST_ID)
        .ok()
        .flatten()
        .filter(|v| !v.is_empty() && v.len() <= 64 && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
        .unwrap_or_else(fresh)
}

pub fn cookie(req: &Request, name: &str) -> Option<String> {
    let raw = req.headers().get("Cookie").ok().flatten()?;
    raw.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

pub async fn json_body<T: serde::de::DeserializeOwned>(req: &mut Request) -> std::result::Result<T, Response> {
    let text = req.text().await.map_err(|_| fail(400, "invalid_body", "corpo ilegível").unwrap())?;
    serde_json::from_str(if text.trim().is_empty() { "{}" } else { &text })
        .map_err(|e| fail(400, "invalid_json", &format!("JSON inválido: {e}")).unwrap())
}
