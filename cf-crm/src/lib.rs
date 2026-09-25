//! Schema e modelo de dados do CRM.
//!
//! Dois bancos:
//! - **D1 global** (`migrations/global`): tenants, usuários, sessões, tokens e o
//!   roteamento `phone_number_id -> tenant` do webhook do Kapso.
//! - **SQLite do Durable Object** (`migrations/tenant`): um banco por tenant com
//!   contatos, funil, conversas e mensagens.
//!
//! O crate não depende de `worker`: compila para wasm32 e para o host (testes).

pub mod dbfmt;
pub mod kapso;
pub mod model;

/// Migration numerada. O runner (`crm_core::migrate`) registra cada versão aplicada
/// na tabela `_migrations` do próprio banco.
pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Migrations do D1 global. Em produção quem aplica é `wrangler d1 migrations apply`;
/// a constante existe para os testes aplicarem o mesmo arquivo.
pub const GLOBAL_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("../migrations/global/0001_init.sql"),
}];

/// Migrations do SQLite de cada tenant. O DO aplica no construtor, em ordem,
/// as que ainda não estão em `_migrations`.
pub const TENANT_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "init",
    sql: include_str!("../migrations/tenant/0001_init.sql"),
}];

/// Janela de atendimento da Meta: 24h desde a última mensagem do contato.
/// Fora dela, só template aprovado.
pub const SERVICE_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// Calculado, nunca gravado (DIRC: Calcular).
pub fn service_window_open(last_inbound_at: Option<i64>, now_ms: i64) -> bool {
    matches!(last_inbound_at, Some(t) if now_ms - t < SERVICE_WINDOW_MS)
}
