//! Timeline do lead. Toda mutação de lead grava uma linha aqui e atualiza
//! `leads.last_activity_at`, na mesma transação da mutação.

use crate::db::{all, exec, json_param, Db};
use crate::{Ctx, Result};
use crm_schema::model::{ActivityType, LeadActivity};
use serde_json::{json, Value};

pub fn record<D: Db>(ctx: &Ctx<D>, lead_id: &str, contact_id: Option<&str>, kind: ActivityType, payload: Value) -> Result<()> {
    exec(
        ctx.db,
        "INSERT INTO lead_activities
           (id, lead_id, contact_id, type, source_module, payload, performed_by, performed_at, created_at)
         VALUES (?, ?, ?, ?, 'crm', ?, ?, ?, ?)",
        &[
            json!(ctx.new_id()),
            json!(lead_id),
            json!(contact_id),
            json!(kind.as_str()),
            json_param(&payload),
            json!(ctx.actor),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    exec(
        ctx.db,
        "UPDATE leads SET last_activity_at = ?, updated_at = ? WHERE id = ?",
        &[json!(ctx.now_ms), json!(ctx.now_ms), json!(lead_id)],
    )
}

/// Mais recentes primeiro. Empate no mesmo milissegundo sai na ordem de gravação.
pub fn list<D: Db>(ctx: &Ctx<D>, lead_id: &str, limit: u32) -> Result<Vec<LeadActivity>> {
    all(
        ctx.db,
        "SELECT * FROM lead_activities WHERE lead_id = ? ORDER BY performed_at DESC, rowid DESC LIMIT ?",
        &[json!(lead_id), json!(limit.clamp(1, 200))],
    )
}
