use crate::activities;
use crate::db::{all, exec, one, Db};
use crate::fractional::key_between;
use crate::pipelines::{self, get_stage};
use crate::{clean, patch, Ctx, CoreError, Result};
use crm_schema::model::{ActivityType, Lead, LeadStatus, Pipeline, Stage};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Default, Deserialize)]
pub struct NewLead {
    pub title: String,
    /// Sem funil, vai pro funil padrão. Sem etapa, vai pra primeira etapa do funil.
    pub pipeline_id: Option<String>,
    pub stage_id: Option<String>,
    pub contact_id: Option<String>,
    pub description: Option<String>,
    pub value_cents: Option<i64>,
    pub currency: Option<String>,
    pub owner_user_id: Option<String>,
    /// `YYYY-MM-DD`
    pub expected_close_date: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LeadPatch {
    pub title: Option<String>,
    #[serde(default, deserialize_with = "patch")]
    pub description: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch")]
    pub value_cents: Option<Option<i64>>,
    pub currency: Option<String>,
    #[serde(default, deserialize_with = "patch")]
    pub owner_user_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch")]
    pub expected_close_date: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch")]
    pub contact_id: Option<Option<String>>,
}

/// Arrastar o card: etapa de destino e, opcionalmente, os vizinhos na coluna.
/// Sem vizinho nenhum, o card vai pro fim da coluna.
#[derive(Debug, Default, Deserialize)]
pub struct MoveLead {
    pub stage_id: String,
    /// Card que fica logo ACIMA do movido.
    pub prev_lead_id: Option<String>,
    /// Card que fica logo ABAIXO do movido.
    pub next_lead_id: Option<String>,
    /// Obrigatório ao entrar numa etapa de perda (se o lead ainda não tiver motivo).
    pub lost_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Board {
    pub pipeline: Pipeline,
    pub columns: Vec<Column>,
}

#[derive(Debug, Serialize)]
pub struct Column {
    pub stage: Stage,
    pub leads: Vec<Lead>,
}

pub fn get<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Lead> {
    one(ctx.db, "SELECT * FROM leads WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("lead"))
}

fn check_currency(c: &str) -> Result<()> {
    if c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(CoreError::validation("invalid_currency", "moeda em ISO-4217, ex.: BRL, EUR, USD"))
    }
}

fn check_date(d: &str) -> Result<()> {
    let b = d.as_bytes();
    let ok = b.len() == 10 && b[4] == b'-' && b[7] == b'-' && d.bytes().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(CoreError::validation("invalid_date", "data no formato AAAA-MM-DD"))
    }
}

fn check_value(v: Option<i64>) -> Result<()> {
    match v {
        Some(n) if n < 0 => Err(CoreError::validation("invalid_value", "valor não pode ser negativo")),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
struct Pos {
    position_in_stage: String,
}

fn last_in_stage<D: Db>(ctx: &Ctx<D>, stage_id: &str, except: Option<&str>) -> Result<Option<String>> {
    Ok(one::<Pos>(
        ctx.db,
        "SELECT position_in_stage FROM leads WHERE stage_id = ? AND id IS NOT ?
         ORDER BY position_in_stage DESC LIMIT 1",
        &[json!(stage_id), json!(except)],
    )?
    .map(|p| p.position_in_stage))
}

pub fn create<D: Db>(ctx: &Ctx<D>, input: NewLead) -> Result<Lead> {
    let title = clean(Some(input.title)).ok_or_else(|| CoreError::validation("title_required", "o lead precisa de título"))?;
    let currency = clean(input.currency).unwrap_or_else(|| "BRL".into());
    check_currency(&currency)?;
    check_value(input.value_cents)?;
    if let Some(d) = &input.expected_close_date {
        check_date(d)?;
    }

    let id = ctx.new_id();
    ctx.db.atomic(|| {
        let stage = match (&input.stage_id, &input.pipeline_id) {
            (Some(sid), _) => get_stage(ctx, sid)?,
            (None, pid) => {
                let pipeline_id = match pid {
                    Some(p) => pipelines::get_pipeline(ctx, p)?.id,
                    None => pipelines::default_pipeline(ctx)?.id,
                };
                pipelines::stages(ctx, &pipeline_id)?
                    .into_iter()
                    .find(|s| !s.is_won && !s.is_lost)
                    .ok_or_else(|| CoreError::validation("no_open_stage", "o funil não tem etapa aberta"))?
            }
        };
        if let Some(pid) = &input.pipeline_id {
            if pid != &stage.pipeline_id {
                return Err(CoreError::validation("stage_not_in_pipeline", "a etapa não pertence a este funil"));
            }
        }
        if stage.is_won || stage.is_lost {
            return Err(CoreError::validation("stage_is_closed", "crie o lead numa etapa aberta e depois mova pra ganho ou perda"));
        }
        let position = key_between(last_in_stage(ctx, &stage.id, None)?.as_deref(), None)?;
        exec(
            ctx.db,
            "INSERT INTO leads (id, pipeline_id, stage_id, contact_id, title, description, status, position_in_stage,
                                value_cents, currency, owner_user_id, assigned_at, expected_close_date,
                                created_by, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 'open', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                json!(id),
                json!(stage.pipeline_id),
                json!(stage.id),
                json!(input.contact_id),
                json!(title),
                json!(clean(input.description)),
                json!(position),
                json!(input.value_cents),
                json!(currency),
                json!(input.owner_user_id),
                json!(input.owner_user_id.as_ref().map(|_| ctx.now_ms)),
                json!(input.expected_close_date),
                json!(ctx.actor),
                json!(ctx.now_ms),
                json!(ctx.now_ms),
            ],
        )?;
        activities::record(
            ctx,
            &id,
            input.contact_id.as_deref(),
            ActivityType::LeadCreated,
            json!({ "stage_id": stage.id, "title": title }),
        )
    })?;
    get(ctx, &id)
}

pub fn update<D: Db>(ctx: &Ctx<D>, id: &str, p: LeadPatch) -> Result<Lead> {
    ctx.db.atomic(|| {
        let cur = get(ctx, id)?;
        let mut changed: Vec<&str> = Vec::new();

        let title = match p.title {
            Some(t) => {
                let t = clean(Some(t)).ok_or_else(|| CoreError::validation("title_required", "o lead precisa de título"))?;
                if t != cur.title {
                    changed.push("title");
                }
                t
            }
            None => cur.title.clone(),
        };
        let description = match p.description {
            Some(d) => {
                let d = clean(d);
                if d != cur.description {
                    changed.push("description");
                }
                d
            }
            None => cur.description.clone(),
        };
        let value_cents = match p.value_cents {
            Some(v) => {
                check_value(v)?;
                if v != cur.value_cents {
                    changed.push("value_cents");
                }
                v
            }
            None => cur.value_cents,
        };
        let currency = match p.currency {
            Some(c) => {
                check_currency(&c)?;
                if c != cur.currency {
                    changed.push("currency");
                }
                c
            }
            None => cur.currency.clone(),
        };
        let expected_close_date = match p.expected_close_date {
            Some(d) => {
                if let Some(d) = &d {
                    check_date(d)?;
                }
                if d != cur.expected_close_date {
                    changed.push("expected_close_date");
                }
                d
            }
            None => cur.expected_close_date.clone(),
        };
        let contact_id = match p.contact_id {
            Some(c) => {
                if c != cur.contact_id {
                    changed.push("contact_id");
                }
                c
            }
            None => cur.contact_id.clone(),
        };
        let (owner, assigned_at) = match p.owner_user_id {
            Some(o) if o != cur.owner_user_id => {
                let at = o.as_ref().map(|_| ctx.now_ms);
                (o, at)
            }
            _ => (cur.owner_user_id.clone(), cur.assigned_at),
        };
        let owner_changed = owner != cur.owner_user_id;

        if changed.is_empty() && !owner_changed {
            return Ok(());
        }
        exec(
            ctx.db,
            "UPDATE leads SET title = ?, description = ?, value_cents = ?, currency = ?, expected_close_date = ?,
                              contact_id = ?, owner_user_id = ?, assigned_at = ?, updated_at = ?
             WHERE id = ?",
            &[
                json!(title),
                json!(description),
                json!(value_cents),
                json!(currency),
                json!(expected_close_date),
                json!(contact_id),
                json!(owner),
                json!(assigned_at),
                json!(ctx.now_ms),
                json!(id),
            ],
        )?;
        if !changed.is_empty() {
            activities::record(ctx, id, contact_id.as_deref(), ActivityType::FieldChanged, json!({ "fields": changed }))?;
        }
        if owner_changed {
            activities::record(
                ctx,
                id,
                contact_id.as_deref(),
                ActivityType::OwnerChanged,
                json!({ "from": cur.owner_user_id, "to": owner }),
            )?;
        }
        Ok(())
    })?;
    get(ctx, id)
}

/// Move o card (de coluna, de posição, ou os dois). A etapa de destino decide o status:
/// etapa de ganho fecha como `won`, de perda como `lost` (pede motivo), aberta reabre.
pub fn move_lead<D: Db>(ctx: &Ctx<D>, id: &str, m: MoveLead) -> Result<Lead> {
    ctx.db.atomic(|| {
        let cur = get(ctx, id)?;
        let stage = get_stage(ctx, &m.stage_id)?;
        if stage.is_archived {
            return Err(CoreError::validation("stage_archived", "etapa arquivada não recebe leads"));
        }

        let neighbor = |nid: &Option<String>| -> Result<Option<Lead>> {
            match nid {
                None => Ok(None),
                Some(n) if n == id => Err(CoreError::validation("invalid_neighbor", "o card não pode ser vizinho de si mesmo")),
                Some(n) => {
                    let l = get(ctx, n)?;
                    if l.stage_id != stage.id {
                        return Err(CoreError::validation("invalid_neighbor", "o vizinho informado está em outra etapa"));
                    }
                    Ok(Some(l))
                }
            }
        };
        let prev = neighbor(&m.prev_lead_id)?;
        let next = neighbor(&m.next_lead_id)?;

        // Só um vizinho: o outro é quem está colado nele na coluna (sem contar o próprio card).
        let prev_key = match (&prev, &next) {
            (Some(p), _) => Some(p.position_in_stage.clone()),
            (None, Some(n)) => one::<Pos>(
                ctx.db,
                "SELECT position_in_stage FROM leads WHERE stage_id = ? AND id <> ? AND position_in_stage < ?
                 ORDER BY position_in_stage DESC LIMIT 1",
                &[json!(stage.id), json!(id), json!(n.position_in_stage)],
            )?
            .map(|p| p.position_in_stage),
            (None, None) => last_in_stage(ctx, &stage.id, Some(id))?,
        };
        let next_key = match (&prev, &next) {
            (_, Some(n)) => Some(n.position_in_stage.clone()),
            (Some(p), None) => one::<Pos>(
                ctx.db,
                "SELECT position_in_stage FROM leads WHERE stage_id = ? AND id <> ? AND position_in_stage > ?
                 ORDER BY position_in_stage ASC LIMIT 1",
                &[json!(stage.id), json!(id), json!(p.position_in_stage)],
            )?
            .map(|p| p.position_in_stage),
            (None, None) => None,
        };
        let position = key_between(prev_key.as_deref(), next_key.as_deref())?;

        let status = if stage.is_won {
            LeadStatus::Won
        } else if stage.is_lost {
            LeadStatus::Lost
        } else {
            LeadStatus::Open
        };
        let lost_reason = match status {
            LeadStatus::Lost => Some(
                clean(m.lost_reason)
                    .or(cur.lost_reason.clone())
                    .ok_or_else(|| CoreError::validation("lost_reason_required", "informe o motivo da perda"))?,
            ),
            _ => None,
        };
        let closed_at = match status {
            LeadStatus::Open => None,
            s if s == cur.status => cur.closed_at,
            _ => Some(ctx.now_ms),
        };

        exec(
            ctx.db,
            "UPDATE leads SET pipeline_id = ?, stage_id = ?, position_in_stage = ?, status = ?, lost_reason = ?,
                              closed_at = ?, updated_at = ?
             WHERE id = ?",
            &[
                json!(stage.pipeline_id),
                json!(stage.id),
                json!(position),
                json!(status.as_str()),
                json!(lost_reason),
                json!(closed_at),
                json!(ctx.now_ms),
                json!(id),
            ],
        )?;

        let contact = cur.contact_id.as_deref();
        if cur.stage_id != stage.id {
            activities::record(
                ctx,
                id,
                contact,
                ActivityType::StageChanged,
                json!({ "from": cur.stage_id, "to": stage.id }),
            )?;
        }
        if cur.status != status {
            activities::record(
                ctx,
                id,
                contact,
                ActivityType::StatusChanged,
                json!({ "from": cur.status.as_str(), "to": status.as_str(), "lost_reason": lost_reason }),
            )?;
        }
        Ok(())
    })?;
    get(ctx, id)
}

pub fn add_note<D: Db>(ctx: &Ctx<D>, id: &str, body: &str) -> Result<()> {
    let body = clean(Some(body.to_string())).ok_or_else(|| CoreError::validation("empty_note", "a nota está vazia"))?;
    ctx.db.atomic(|| {
        let lead = get(ctx, id)?;
        activities::record(ctx, id, lead.contact_id.as_deref(), ActivityType::NoteAdded, json!({ "body": body }))
    })
}

/// Kanban de um funil: etapas em ordem, cada uma com seus cards em ordem.
pub fn board<D: Db>(ctx: &Ctx<D>, pipeline_id: &str) -> Result<Board> {
    let pipeline = pipelines::get_pipeline(ctx, pipeline_id)?;
    let stages = pipelines::stages(ctx, pipeline_id)?;
    let mut leads: Vec<Lead> = all(
        ctx.db,
        "SELECT * FROM leads WHERE pipeline_id = ? ORDER BY stage_id, position_in_stage",
        &[json!(pipeline_id)],
    )?;
    let columns = stages
        .into_iter()
        .map(|stage| {
            let (mine, rest): (Vec<Lead>, Vec<Lead>) = leads.drain(..).partition(|l| l.stage_id == stage.id);
            leads = rest;
            Column { stage, leads: mine }
        })
        .collect();
    Ok(Board { pipeline, columns })
}
