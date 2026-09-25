use crate::db::{all, exec, json_param, one, Db};
use crate::fractional::key_between;
use crate::{clean, valid_slug, Ctx, CoreError, Result};
use crm_schema::model::{Pipeline, Stage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub struct NewPipeline {
    pub name: String,
    pub slug: String,
    pub description: Option<String>,
    /// Sem etapas, o funil nasce com o padrão (novo → qualificado → proposta → ganho / perdido).
    pub stages: Option<Vec<NewStage>>,
    pub vocabulary: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewStage {
    pub name: String,
    pub slug: String,
    pub color: Option<String>,
    #[serde(default)]
    pub is_won: bool,
    #[serde(default)]
    pub is_lost: bool,
    #[serde(default)]
    pub requires_human: bool,
}

#[derive(Debug, Serialize)]
pub struct PipelineWithStages {
    #[serde(flatten)]
    pub pipeline: Pipeline,
    pub stages: Vec<Stage>,
}

fn default_stages() -> Vec<NewStage> {
    let s = |name: &str, slug: &str, won: bool, lost: bool| NewStage {
        name: name.into(),
        slug: slug.into(),
        color: None,
        is_won: won,
        is_lost: lost,
        requires_human: false,
    };
    vec![
        s("Novo", "novo", false, false),
        s("Qualificado", "qualificado", false, false),
        s("Proposta", "proposta", false, false),
        s("Ganho", "ganho", true, false),
        s("Perdido", "perdido", false, true),
    ]
}

#[derive(Deserialize)]
struct Pos {
    position: String,
}

fn last_position<D: Db>(ctx: &Ctx<D>, sql: &str, params: &[Value]) -> Result<Option<String>> {
    Ok(one::<Pos>(ctx.db, sql, params)?.map(|p| p.position))
}

pub fn create<D: Db>(ctx: &Ctx<D>, input: NewPipeline) -> Result<PipelineWithStages> {
    let name = clean(Some(input.name)).ok_or_else(|| CoreError::validation("name_required", "o funil precisa de nome"))?;
    valid_slug(&input.slug)?;
    let stages = input.stages.unwrap_or_else(default_stages);
    if stages.is_empty() {
        return Err(CoreError::validation("stages_required", "o funil precisa de ao menos uma etapa"));
    }
    let id = ctx.new_id();
    ctx.db.atomic(|| {
        #[derive(Deserialize)]
        struct N {
            n: i64,
        }
        let is_first = one::<N>(ctx.db, "SELECT count(*) AS n FROM pipelines", &[])?.map_or(0, |r| r.n) == 0;
        let after = last_position(ctx, "SELECT position FROM pipelines ORDER BY position DESC LIMIT 1", &[])?;
        exec(
            ctx.db,
            "INSERT INTO pipelines (id, name, slug, description, is_default, position, vocabulary, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                json!(id),
                json!(name),
                json!(input.slug),
                json!(clean(input.description)),
                json!(is_first),
                json!(key_between(after.as_deref(), None)?),
                json_param(&input.vocabulary.unwrap_or_else(|| json!({}))),
                json!(ctx.now_ms),
                json!(ctx.now_ms),
            ],
        )?;
        for s in stages {
            insert_stage(ctx, &id, s)?;
        }
        Ok(())
    })?;
    get(ctx, &id)
}

fn insert_stage<D: Db>(ctx: &Ctx<D>, pipeline_id: &str, s: NewStage) -> Result<Stage> {
    let name = clean(Some(s.name)).ok_or_else(|| CoreError::validation("name_required", "a etapa precisa de nome"))?;
    valid_slug(&s.slug)?;
    if s.is_won && s.is_lost {
        return Err(CoreError::validation("stage_won_and_lost", "uma etapa não pode ser de ganho e de perda ao mesmo tempo"));
    }
    let after = last_position(
        ctx,
        "SELECT position FROM stages WHERE pipeline_id = ? ORDER BY position DESC LIMIT 1",
        &[json!(pipeline_id)],
    )?;
    let id = ctx.new_id();
    exec(
        ctx.db,
        "INSERT INTO stages (id, pipeline_id, name, slug, position, color, is_won, is_lost, requires_human, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            json!(id),
            json!(pipeline_id),
            json!(name),
            json!(s.slug),
            json!(key_between(after.as_deref(), None)?),
            json!(clean(s.color)),
            json!(s.is_won),
            json!(s.is_lost),
            json!(s.requires_human),
            json!(ctx.now_ms),
            json!(ctx.now_ms),
        ],
    )?;
    get_stage(ctx, &id)
}

pub fn add_stage<D: Db>(ctx: &Ctx<D>, pipeline_id: &str, s: NewStage) -> Result<Stage> {
    get_pipeline(ctx, pipeline_id)?;
    ctx.db.atomic(|| insert_stage(ctx, pipeline_id, s))
}

pub fn get_pipeline<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Pipeline> {
    one(ctx.db, "SELECT * FROM pipelines WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("funil"))
}

pub fn get_stage<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<Stage> {
    one(ctx.db, "SELECT * FROM stages WHERE id = ?", &[json!(id)])?.ok_or(CoreError::NotFound("etapa"))
}

pub fn stages<D: Db>(ctx: &Ctx<D>, pipeline_id: &str) -> Result<Vec<Stage>> {
    all(
        ctx.db,
        "SELECT * FROM stages WHERE pipeline_id = ? AND is_archived = 0 ORDER BY position",
        &[json!(pipeline_id)],
    )
}

pub fn get<D: Db>(ctx: &Ctx<D>, id: &str) -> Result<PipelineWithStages> {
    Ok(PipelineWithStages { pipeline: get_pipeline(ctx, id)?, stages: stages(ctx, id)? })
}

pub fn list<D: Db>(ctx: &Ctx<D>) -> Result<Vec<PipelineWithStages>> {
    let pipelines: Vec<Pipeline> = all(ctx.db, "SELECT * FROM pipelines WHERE is_archived = 0 ORDER BY position", &[])?;
    pipelines
        .into_iter()
        .map(|p| Ok(PipelineWithStages { stages: stages(ctx, &p.id)?, pipeline: p }))
        .collect()
}

pub fn default_pipeline<D: Db>(ctx: &Ctx<D>) -> Result<Pipeline> {
    one(ctx.db, "SELECT * FROM pipelines WHERE is_default = 1 AND is_archived = 0", &[])?
        .ok_or_else(|| CoreError::validation("no_default_pipeline", "crie um funil antes de criar leads"))
}
