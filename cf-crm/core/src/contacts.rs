use crate::db::{all, exec, one, Db};
use crate::{clean, patch, Ctx, CoreError, Page, Result};
use crm_schema::model::Contact;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Default, Deserialize)]
pub struct NewContact {
    pub name: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub source: Option<String>,
}

/// Ausente = não mexe; `null` = apaga.
#[derive(Debug, Default, Deserialize)]
pub struct ContactPatch {
    #[serde(default, deserialize_with = "patch")]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch")]
    pub email: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch")]
    pub phone: Option<Option<String>>,
}

/// Aceita o que as pessoas digitam ("+55 (11) 98765-4321", "0055...") e devolve E.164.
/// Não inventa DDI: número sem `+` ou `00` é recusado, porque o CRM atende três países.
pub fn normalize_phone(raw: &str) -> Result<String> {
    let compact: String = raw.chars().filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '.')).collect();
    let digits = compact
        .strip_prefix('+')
        .or_else(|| compact.strip_prefix("00"))
        .ok_or_else(|| CoreError::validation("invalid_phone", "telefone precisa do código do país: +55, +351, +1..."))?;
    let ok = (8..=15).contains(&digits.len()) && digits.bytes().all(|c| c.is_ascii_digit()) && !digits.starts_with('0');
    if !ok {
        return Err(CoreError::validation("invalid_phone", format!("telefone inválido: {raw}")));
    }
    Ok(format!("+{digits}"))
}

pub fn normalize_email(raw: &str) -> Result<String> {
    let e = raw.trim();
    let valid = match e.split_once('@') {
        Some((user, domain)) => !user.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.') && !e.contains(char::is_whitespace),
        None => false,
    };
    if valid {
        Ok(e.to_string())
    } else {
        Err(CoreError::validation("invalid_email", format!("e-mail inválido: {raw}")))
    }
}

pub fn create<D: Db>(ctx: &Ctx<D>, input: NewContact) -> Result<Contact> {
    let name = clean(input.name);
    let email = clean(input.email).map(|e| normalize_email(&e)).transpose()?;
    let phone = clean(input.phone).map(|p| normalize_phone(&p)).transpose()?;
    if name.is_none() && email.is_none() && phone.is_none() {
        return Err(CoreError::validation("empty_contact", "informe ao menos nome, e-mail ou telefone"));
    }
    let id = ctx.new_id();
    exec(
        ctx.db,
        "INSERT INTO contacts (id, name, email, phone_e164, source, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            json!(id),
            json!(name),
            json!(email),
            json!(phone),
            json!(clean(input.source).unwrap_or_else(|| "manual".into())),
            json!(ctx.actor),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    get(ctx, &id)
}

pub fn get<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Contact> {
    one(ctx.db, "SELECT * FROM contacts WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("contato"))
}

/// Mais recentes primeiro. `q` procura em nome, e-mail e telefone.
/// Mesclados e anonimizados ficam de fora.
pub fn list<D: Db>(ctx: &Ctx<D>, limit: u32, cursor: Option<&str>, q: Option<&str>) -> Result<Page<Contact>> {
    let limit = limit.clamp(1, 100);
    let (c_at, c_id) = match cursor {
        Some(c) => {
            let (t, id) = c.split_once('.').ok_or_else(|| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            let t: i64 = t.parse().map_err(|_| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            (json!(t), json!(id))
        }
        None => (Value::Null, Value::Null),
    };
    let like = q.map(|q| format!("%{}%", q.trim().replace('%', "\\%").replace('_', "\\_")));
    let mut items: Vec<Contact> = all(
        ctx.db,
        "SELECT * FROM contacts
         WHERE merged_into_id IS NULL AND is_anonymized = 0
           AND (?1 IS NULL OR (created_at, id) < (?1, ?2))
           AND (?3 IS NULL OR name LIKE ?3 ESCAPE '\\' OR email LIKE ?3 ESCAPE '\\' OR phone_e164 LIKE ?3 ESCAPE '\\')
         ORDER BY created_at DESC, id DESC
         LIMIT ?4",
        &[c_at, c_id, json!(like), json!(limit + 1)],
    )?;
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|c| format!("{}.{}", c.created_at, c.id))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

pub fn update<D: Db>(ctx: &Ctx<D>, id: &str, p: ContactPatch) -> Result<Contact> {
    let current = get(ctx, id)?;
    if current.is_anonymized {
        return Err(CoreError::conflict("contact_anonymized", "contato anonimizado não pode ser editado"));
    }
    let name = match p.name {
        Some(v) => clean(v),
        None => current.name,
    };
    let email = match p.email {
        Some(v) => clean(v).map(|e| normalize_email(&e)).transpose()?,
        None => current.email,
    };
    let phone = match p.phone {
        Some(v) => clean(v).map(|x| normalize_phone(&x)).transpose()?,
        None => current.phone_e164,
    };
    if name.is_none() && email.is_none() && phone.is_none() {
        return Err(CoreError::validation("empty_contact", "o contato precisa manter ao menos nome, e-mail ou telefone"));
    }
    exec(
        ctx.db,
        "UPDATE contacts SET name = ?, email = ?, phone_e164 = ?, updated_at = ? WHERE id = ?",
        &[json!(name), json!(email), json!(phone), json!(ctx.now_ms), json!(id)],
    )?;
    get(ctx, id)
}
