//! Cliente HTTP para `/api/v1/*`. O front roda na mesma origem do Worker
//! (ver `wrangler.toml` -> `[assets]`), então o cookie de sessão
//! (`HttpOnly; Secure; SameSite=Strict`) viaja sozinho em toda `fetch`
//! same-origin — não precisamos (nem conseguimos) lê-lo aqui.

use gloo_net::http::Request;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

pub const TENANT_HEADER: &str = "X-Tenant-Id";

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Option<T>,
    error: Option<ErrorBody>,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Membership {
    pub tenant_id: String,
    pub role: String,
    pub slug: String,
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub user: User,
    pub tenants: Vec<Membership>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub contact_id: String,
    pub phone_number_id: String,
    pub status: String,
    pub last_message_at: Option<i64>,
    pub last_message_preview: Option<String>,
    pub unread_count: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct InboxItem {
    #[serde(flatten)]
    pub conversation: Conversation,
    pub contact_name: Option<String>,
    pub contact_phone: Option<String>,
    pub service_window_open: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub direction: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub body: Option<String>,
    pub sent_via: String,
    pub error_message: Option<String>,
    pub sent_at: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Pipeline {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PipelineWithStages {
    #[serde(flatten)]
    pub pipeline: Pipeline,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Stage {
    pub id: String,
    pub pipeline_id: String,
    pub name: String,
    pub is_won: bool,
    pub is_lost: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Lead {
    pub id: String,
    pub pipeline_id: String,
    pub stage_id: String,
    pub title: String,
    pub status: String,
    pub value_cents: Option<i64>,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Column {
    pub stage: Stage,
    pub leads: Vec<Lead>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Board {
    pub pipeline: Pipeline,
    pub columns: Vec<Column>,
}

async fn envelope<T: DeserializeOwned>(resp: gloo_net::http::Response) -> Result<T, ApiError> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Envelope<T> = serde_json::from_str(&text).map_err(|e| ApiError {
        status,
        code: "bad_response".into(),
        message: format!("resposta inesperada do servidor: {e}"),
    })?;
    if let Some(err) = parsed.error {
        return Err(ApiError { status, code: err.code, message: err.message });
    }
    parsed.data.ok_or(ApiError {
        status,
        code: "empty_response".into(),
        message: "resposta sem dado".into(),
    })
}

fn net_err(e: gloo_net::Error) -> ApiError {
    ApiError { status: 0, code: "network_error".into(), message: e.to_string() }
}

pub async fn login(email: &str, password: &str) -> Result<(), ApiError> {
    let resp = Request::post("/api/v1/auth/login")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "email": email, "password": password }))
        .map_err(net_err)?
        .send()
        .await
        .map_err(net_err)?;
    envelope::<Value>(resp).await.map(|_| ())
}

pub async fn logout() -> Result<(), ApiError> {
    let resp = Request::post("/api/v1/auth/logout").send().await.map_err(net_err)?;
    envelope::<Value>(resp).await.map(|_| ())
}

pub async fn me() -> Result<Me, ApiError> {
    let resp = Request::get("/api/v1/me").send().await.map_err(net_err)?;
    envelope(resp).await
}

pub async fn list_conversations(tenant_id: &str, cursor: Option<&str>) -> Result<Page<InboxItem>, ApiError> {
    let mut url = "/api/v1/conversations?limit=30".to_string();
    if let Some(c) = cursor {
        url.push_str("&cursor=");
        url.push_str(c);
    }
    let resp = Request::get(&url)
        .header(TENANT_HEADER, tenant_id)
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn list_messages(tenant_id: &str, conversation_id: &str) -> Result<Page<Message>, ApiError> {
    let url = format!("/api/v1/conversations/{conversation_id}/messages?limit=50");
    let resp = Request::get(&url)
        .header(TENANT_HEADER, tenant_id)
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn send_text(tenant_id: &str, conversation_id: &str, body: &str) -> Result<Message, ApiError> {
    let url = format!("/api/v1/conversations/{conversation_id}/messages");
    let resp = Request::post(&url)
        .header("Content-Type", "application/json")
        .header(TENANT_HEADER, tenant_id)
        .json(&serde_json::json!({ "type": "text", "body": body }))
        .map_err(net_err)?
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn mark_read(tenant_id: &str, conversation_id: &str) -> Result<(), ApiError> {
    let url = format!("/api/v1/conversations/{conversation_id}/read");
    // POST sem `Content-Type: application/json` leva 415 do proxy do Worker
    // (barra POST de formulário cross-site), mesmo sem corpo de verdade.
    let resp = Request::post(&url)
        .header("Content-Type", "application/json")
        .header(TENANT_HEADER, tenant_id)
        .body("{}")
        .map_err(net_err)?
        .send()
        .await
        .map_err(net_err)?;
    envelope::<Value>(resp).await.map(|_| ())
}

pub async fn list_pipelines(tenant_id: &str) -> Result<Vec<PipelineWithStages>, ApiError> {
    let resp = Request::get("/api/v1/pipelines")
        .header(TENANT_HEADER, tenant_id)
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn get_board(tenant_id: &str, pipeline_id: &str) -> Result<Board, ApiError> {
    let url = format!("/api/v1/pipelines/{pipeline_id}/board");
    let resp = Request::get(&url)
        .header(TENANT_HEADER, tenant_id)
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn create_lead(tenant_id: &str, title: &str, stage_id: &str) -> Result<Lead, ApiError> {
    let resp = Request::post("/api/v1/leads")
        .header("Content-Type", "application/json")
        .header(TENANT_HEADER, tenant_id)
        .json(&serde_json::json!({ "title": title, "stage_id": stage_id }))
        .map_err(net_err)?
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}

pub async fn move_lead(
    tenant_id: &str,
    lead_id: &str,
    stage_id: &str,
    lost_reason: Option<&str>,
) -> Result<Lead, ApiError> {
    let url = format!("/api/v1/leads/{lead_id}/move");
    let resp = Request::post(&url)
        .header("Content-Type", "application/json")
        .header(TENANT_HEADER, tenant_id)
        .json(&serde_json::json!({ "stage_id": stage_id, "lost_reason": lost_reason }))
        .map_err(net_err)?
        .send()
        .await
        .map_err(net_err)?;
    envelope(resp).await
}
