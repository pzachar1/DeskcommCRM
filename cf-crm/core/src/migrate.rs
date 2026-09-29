use crate::db::{exec, one, Db};
use crate::error::{CoreError, Result};
use crm_schema::Migration;
use serde::Deserialize;
use serde_json::json;

/// Aplica, em ordem, as migrations que ainda não estão em `_migrations`.
/// Cada uma roda numa transação própria: ou entra inteira, ou não entra.
/// Devolve a versão em que o banco ficou.
pub fn migrate(db: &impl Db, migrations: &[Migration], now_ms: i64) -> Result<u32> {
    db.batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
           version    INTEGER PRIMARY KEY,
           name       TEXT NOT NULL,
           applied_at INTEGER NOT NULL
         )",
    )?;

    #[derive(Deserialize)]
    struct Current {
        v: Option<u32>,
    }
    let applied = one::<Current>(db, "SELECT max(version) AS v FROM _migrations", &[])?
        .and_then(|c| c.v)
        .unwrap_or(0);

    let mut current = applied;
    for m in migrations.iter().filter(|m| m.version > applied) {
        db.atomic(|| {
            db.batch(m.sql)?;
            exec(
                db,
                "INSERT INTO _migrations (version, name, applied_at) VALUES (?, ?, ?)",
                &[json!(m.version), json!(m.name), json!(now_ms)],
            )
        })?;
        current = m.version;
    }
    Ok(current)
}

/// Grava o tenant dono deste banco na primeira vez e confere nas seguintes.
/// Se um erro de roteamento mandar o tenant A para o DO do tenant B, para aqui.
pub fn ensure_tenant(db: &impl Db, tenant_id: &str) -> Result<()> {
    exec(
        db,
        "INSERT INTO meta (key, value) VALUES ('tenant_id', ?) ON CONFLICT (key) DO NOTHING",
        &[json!(tenant_id)],
    )?;
    #[derive(Deserialize)]
    struct Meta {
        value: String,
    }
    let stored = one::<Meta>(db, "SELECT value FROM meta WHERE key = 'tenant_id'", &[])?
        .ok_or(CoreError::Db("meta.tenant_id sumiu".into()))?;
    if stored.value != tenant_id {
        return Err(CoreError::conflict("tenant_mismatch", "requisição roteada para o banco de outro tenant"));
    }
    Ok(())
}
