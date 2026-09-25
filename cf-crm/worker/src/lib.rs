//! Worker de entrada. Autentica no D1, resolve o tenant, confere o papel e repassa
//! para o Durable Object do tenant. Não guarda dado de negócio.

mod auth;
mod http;
mod ids;
mod tenant;

pub use tenant::TenantDo;

use crm_schema::model::Role;
use http::{fail, ok, request_id, with_request_id};
use ids::{now_ms, uuid_v7};
use serde_json::json;
use tenant::{ACTOR_HEADER, TENANT_HEADER};
use worker::{event, js_sys, Context, Env, Headers, Method, Request, RequestInit, Response, Result};

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let rid = request_id(&req, || uuid_v7(now_ms()));
    let resp = match handle(req, &env, &rid).await {
        Ok(r) => r,
        Err(e) => {
            worker::console_error!("[{rid}] {e}");
            fail(500, "internal_error", "erro interno")?
        }
    };
    with_request_id(resp, &rid)
}

async fn handle(req: Request, env: &Env, rid: &str) -> Result<Response> {
    let path = req.path();
    match (req.method(), path.as_str()) {
        (Method::Get, "/health") => ok(&json!({ "status": "ok" }), 200),
        (Method::Post, "/api/v1/auth/signup") => {
            let (resp, created) = auth::signup(req, env).await?;
            if let Some((tenant_id, user_id)) = created {
                // a conta nova já abre com um funil pronto pra usar
                let body = json!({ "name": "Vendas", "slug": "vendas" }).to_string();
                if let Err(e) = to_tenant(env, &tenant_id, &user_id, rid, Method::Post, "/api/v1/pipelines", Some(body.into_bytes())).await {
                    worker::console_error!("[{rid}] funil inicial não foi criado: {e}");
                }
            }
            Ok(resp)
        }
        (Method::Post, "/api/v1/auth/login") => auth::login(req, env).await,
        (Method::Post, "/api/v1/auth/logout") => auth::logout(req, env).await,
        (Method::Get, "/api/v1/me") => me(req, env).await,
        (_, p) if p.starts_with("/api/v1/") => forward(req, env, rid).await,
        _ => fail(404, "route_not_found", "rota não existe"),
    }
}

async fn me(req: Request, env: &Env) -> Result<Response> {
    let db = env.d1("DB")?;
    let Some(s) = auth::current_session(&req, &db).await? else {
        return fail(401, "unauthenticated", "faça login");
    };
    let tenants = auth::memberships(&db, &s.user_id).await?;
    ok(&json!({ "user": { "id": s.user_id, "email": s.email, "name": s.name }, "tenants": tenants }), 200)
}

/// Leitura pede viewer; escrita pede agent; mexer em funil pede manager.
fn required_role(method: &Method, path: &str) -> Role {
    match method {
        Method::Get | Method::Head => Role::Viewer,
        _ if path.starts_with("/api/v1/pipelines") => Role::Manager,
        _ => Role::Agent,
    }
}

async fn forward(mut req: Request, env: &Env, rid: &str) -> Result<Response> {
    let db = env.d1("DB")?;
    let Some(session) = auth::current_session(&req, &db).await? else {
        return fail(401, "unauthenticated", "faça login");
    };
    let memberships = auth::memberships(&db, &session.user_id).await?;
    let wanted = req.headers().get(TENANT_HEADER)?;
    let membership = match wanted {
        Some(t) => memberships.iter().find(|m| m.tenant_id == t),
        None if memberships.len() == 1 => memberships.first(),
        None => return fail(400, "tenant_required", "informe o tenant no header X-Tenant-Id"),
    };
    let Some(m) = membership else {
        return fail(403, "forbidden_tenant", "você não tem acesso a este tenant");
    };
    if m.status != "active" {
        return fail(403, "tenant_inactive", "tenant suspenso ou encerrado");
    }
    let method = req.method();
    let path = req.path();
    if !m.role().at_least(required_role(&method, &path)) {
        return fail(403, "insufficient_role", "seu papel não permite esta ação");
    }
    let body = match method {
        Method::Get | Method::Head => None,
        _ => {
            // barra POST de formulário cross-site (que não consegue mandar este content-type)
            let ct = req.headers().get("Content-Type")?.unwrap_or_default();
            if !ct.starts_with("application/json") {
                return fail(415, "unsupported_media_type", "envie Content-Type: application/json");
            }
            Some(req.bytes().await?)
        }
    };
    let path_q = match req.url()?.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    };
    to_tenant(env, &m.tenant_id, &session.user_id, rid, method, &path_q, body).await
}

/// Monta a requisição interna do zero: nenhum header do cliente passa adiante,
/// então ninguém de fora consegue forjar `X-Actor-User-Id`.
async fn to_tenant(env: &Env, tenant_id: &str, user_id: &str, rid: &str, method: Method, path_q: &str, body: Option<Vec<u8>>) -> Result<Response> {
    let headers = Headers::new();
    headers.set(TENANT_HEADER, tenant_id)?;
    headers.set(ACTOR_HEADER, user_id)?;
    headers.set(http::REQUEST_ID, rid)?;
    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(b) = body {
        init.with_body(Some(js_sys::Uint8Array::from(b.as_slice()).into()));
    }
    let inner = Request::new_with_init(&format!("https://tenant.internal{path_q}"), &init)?;
    let stub = env.durable_object("TENANT")?.id_from_name(tenant_id)?.get_stub()?;
    let mut resp = stub.fetch_with_request(inner).await?;
    // Resposta vinda de outro objeto tem headers imutáveis; remonta pra poder
    // acrescentar X-Request-Id na saída.
    let status = resp.status_code();
    let out_headers = Headers::new();
    if let Some(ct) = resp.headers().get("Content-Type")? {
        out_headers.set("Content-Type", &ct)?;
    }
    Ok(Response::from_bytes(resp.bytes().await?)?.with_status(status).with_headers(out_headers))
}
