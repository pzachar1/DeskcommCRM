//! Regras de negócio do CRM, sem runtime. Tudo aqui roda igual no Durable Object
//! (wasm) e nos testes (rusqlite no host): o banco entra pelo trait [`db::Db`],
//! o relógio e o gerador de id entram pelo [`Ctx`].

pub mod activities;
pub mod contacts;
pub mod db;
pub mod error;
pub mod fractional;
pub mod leads;
pub mod migrate;
pub mod pipelines;

pub use error::{CoreError, Result};

use db::Db;
use serde::{Deserialize, Deserializer};

/// Contexto de uma requisição: banco, instante (um só por requisição, então todas
/// as linhas escritas juntas têm o mesmo carimbo) e quem está agindo.
pub struct Ctx<'a, D: Db> {
    pub db: &'a D,
    pub now_ms: i64,
    /// user_id do D1; None = sistema (webhook, automação).
    pub actor: Option<String>,
    new_id: &'a dyn Fn() -> String,
}

impl<'a, D: Db> Ctx<'a, D> {
    pub fn new(db: &'a D, now_ms: i64, actor: Option<String>, new_id: &'a dyn Fn() -> String) -> Self {
        Ctx { db, now_ms, actor, new_id }
    }

    pub fn new_id(&self) -> String {
        (self.new_id)()
    }
}

/// Página de listagem com cursor opaco.
#[derive(Debug, serde::Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

/// Campo de PATCH que distingue "não mandou" (None) de "mandou null" (Some(None)).
/// Usar com `#[serde(default, deserialize_with = "crate::patch")]`.
pub fn patch<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

pub(crate) fn clean(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub fn valid_slug(slug: &str) -> Result<()> {
    let ok = (2..=40).contains(&slug.len())
        && slug.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-');
    if ok {
        Ok(())
    } else {
        Err(CoreError::validation("invalid_slug", "slug deve ter de 2 a 40 caracteres: a-z, 0-9, _ ou -"))
    }
}
