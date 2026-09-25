//! Conta, sessão e vínculo com tenant. Tudo no D1 global.

use crate::http::{cookie, fail, json_body, ok};
use crate::ids::{now_ms, random_bytes, session_token, sha256_hex, uuid_v7};
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use crm_schema::model::Role;
use serde::{Deserialize, Serialize};
use worker::wasm_bindgen::JsValue;
use worker::{D1Database, Env, Request, Response, Result};

pub const COOKIE: &str = "crm_session";
const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MIN_PASSWORD: usize = 10;

#[derive(Debug, Deserialize)]
pub struct Session {
    pub user_id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Membership {
    pub tenant_id: String,
    pub role: String,
    pub slug: String,
    pub name: String,
    pub status: String,
}

impl Membership {
    pub fn role(&self) -> Role {
        Role::parse(&self.role).unwrap_or(Role::Viewer)
    }
}

fn s(v: &str) -> JsValue {
    JsValue::from_str(v)
}

fn n(v: i64) -> JsValue {
    JsValue::from_f64(v as f64)
}

fn hash_password(password: &str) -> std::result::Result<String, String> {
    let salt = SaltString::encode_b64(&random_bytes::<16>()).map_err(|e| e.to_string())?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_password(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

fn session_cookie(token: &str, max_age_s: i64) -> String {
    format!("{COOKIE}={token}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={max_age_s}")
}

async fn open_session(db: &D1Database, user_id: &str) -> Result<String> {
    let token = session_token();
    let now = now_ms();
    db.prepare("INSERT INTO sessions (token_sha256, user_id, expires_at, created_at, last_seen_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&[s(&sha256_hex(&token)), s(user_id), n(now + SESSION_TTL_MS), n(now), n(now)])?
        .run()
        .await?;
    Ok(token)
}

/// Sessão válida a partir do cookie. Sessão vencida conta como ausente.
pub async fn current_session(req: &Request, db: &D1Database) -> Result<Option<Session>> {
    let Some(token) = cookie(req, COOKIE) else { return Ok(None) };
    db.prepare(
        "SELECT u.id AS user_id, u.email, u.name
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_sha256 = ? AND s.expires_at > ?",
    )
    .bind(&[s(&sha256_hex(&token)), n(now_ms())])?
    .first::<Session>(None)
    .await
}

pub async fn memberships(db: &D1Database, user_id: &str) -> Result<Vec<Membership>> {
    db.prepare(
        "SELECT m.tenant_id, m.role, t.slug, t.name, t.status
         FROM memberships m JOIN tenants t ON t.id = m.tenant_id
         WHERE m.user_id = ? ORDER BY t.name",
    )
    .bind(&[s(user_id)])?
    .all()
    .await?
    .results::<Membership>()
}

#[derive(Deserialize)]
struct SignupInput {
    email: String,
    password: String,
    name: Option<String>,
    tenant_name: String,
    tenant_slug: String,
}

/// Cria usuário + tenant + vínculo admin num batch do D1 (atômico).
/// Fechado por padrão: só abre com `ALLOW_SIGNUP = "true"`.
/// Devolve `(tenant_id, user_id)` quando criou, pra quem chamou semear o tenant.
pub async fn signup(mut req: Request, env: &Env) -> Result<(Response, Option<(String, String)>)> {
    if env.var("ALLOW_SIGNUP").map(|v| v.to_string()).unwrap_or_default() != "true" {
        return Ok((fail(403, "signup_disabled", "cadastro fechado nesta instalação")?, None));
    }
    let input: SignupInput = match json_body(&mut req).await {
        Ok(v) => v,
        Err(resp) => return Ok((resp, None)),
    };
    let email = match crm_core::contacts::normalize_email(&input.email) {
        Ok(e) => e.to_lowercase(),
        Err(e) => return Ok((crate::http::from_core(&e)?, None)),
    };
    if input.password.chars().count() < MIN_PASSWORD {
        return Ok((fail(422, "weak_password", "a senha precisa de pelo menos 10 caracteres")?, None));
    }
    if let Err(e) = crm_core::valid_slug(&input.tenant_slug) {
        return Ok((crate::http::from_core(&e)?, None));
    }
    let tenant_name = input.tenant_name.trim();
    if tenant_name.is_empty() {
        return Ok((fail(422, "tenant_name_required", "informe o nome da empresa")?, None));
    }

    let db = env.d1("DB")?;
    let hash = hash_password(&input.password).map_err(worker::Error::RustError)?;
    let now = now_ms();
    let (user_id, tenant_id) = (uuid_v7(now), uuid_v7(now));
    let name = input.name.as_deref().map(str::trim).filter(|v| !v.is_empty());

    let batch = vec![
        db.prepare("INSERT INTO users (id, email, name, password_hash, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&[s(&user_id), s(&email), name.map(s).unwrap_or(JsValue::NULL), s(&hash), n(now), n(now)])?,
        db.prepare("INSERT INTO tenants (id, slug, name, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&[s(&tenant_id), s(&input.tenant_slug), s(tenant_name), n(now), n(now)])?,
        db.prepare("INSERT INTO memberships (tenant_id, user_id, role, created_at) VALUES (?, ?, 'admin', ?)")
            .bind(&[s(&tenant_id), s(&user_id), n(now)])?,
    ];
    if let Err(e) = db.batch(batch).await {
        let msg = e.to_string();
        if msg.contains("UNIQUE constraint failed") {
            let what = if msg.contains("users.email") { "e-mail" } else { "endereço (slug) da empresa" };
            return Ok((fail(409, "already_exists", &format!("{what} já está em uso"))?, None));
        }
        return Err(e);
    }

    let token = open_session(&db, &user_id).await?;
    let mut resp = ok(&serde_json::json!({ "user_id": user_id, "tenant_id": tenant_id }), 201)?;
    resp.headers_mut().set("Set-Cookie", &session_cookie(&token, SESSION_TTL_MS / 1000))?;
    Ok((resp, Some((tenant_id, user_id))))
}

#[derive(Deserialize)]
struct LoginInput {
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct UserRow {
    id: String,
    password_hash: Option<String>,
}

pub async fn login(mut req: Request, env: &Env) -> Result<Response> {
    let input: LoginInput = match json_body(&mut req).await {
        Ok(v) => v,
        Err(resp) => return Ok(resp),
    };
    let db = env.d1("DB")?;
    let user = db
        .prepare("SELECT id, password_hash FROM users WHERE email = ?")
        .bind(&[s(input.email.trim())])?
        .first::<UserRow>(None)
        .await?;

    let valid = match &user {
        Some(UserRow { password_hash: Some(h), .. }) => verify_password(&input.password, h),
        _ => {
            // mesmo custo de CPU com ou sem conta, pra não revelar quais e-mails existem
            let _ = hash_password(&input.password);
            false
        }
    };
    let Some(user) = user.filter(|_| valid) else {
        return fail(401, "invalid_credentials", "e-mail ou senha incorretos");
    };
    let token = open_session(&db, &user.id).await?;
    let mut resp = ok(&serde_json::json!({ "user_id": user.id }), 200)?;
    resp.headers_mut().set("Set-Cookie", &session_cookie(&token, SESSION_TTL_MS / 1000))?;
    Ok(resp)
}

pub async fn logout(req: Request, env: &Env) -> Result<Response> {
    if let Some(token) = cookie(&req, COOKIE) {
        env.d1("DB")?
            .prepare("DELETE FROM sessions WHERE token_sha256 = ?")
            .bind(&[s(&sha256_hex(&token))])?
            .run()
            .await?;
    }
    let mut resp = ok(&serde_json::json!({ "logged_out": true }), 200)?;
    resp.headers_mut().set("Set-Cookie", &session_cookie("", 0))?;
    Ok(resp)
}
