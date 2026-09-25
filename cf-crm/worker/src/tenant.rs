//! Durable Object de um tenant: dono do SQLite com os dados de negócio.
//! Só é alcançável pela binding `TENANT`, então os headers `X-Tenant-Id` e
//! `X-Actor-User-Id` vêm do Worker de entrada, que já autenticou.

use crate::http::{fail, from_core, json_body, ok};
use crate::ids::{now_ms, uuid_v7};
use crm_core::contacts::{self, ContactPatch, NewContact};
use crm_core::db::{Db, Row};
use crm_core::leads::{self, LeadPatch, MoveLead, NewLead};
use crm_core::migrate::{ensure_tenant, migrate};
use crm_core::pipelines::{self, NewPipeline, NewStage};
use crm_core::{activities, CoreError, Ctx};
use crm_schema::TENANT_MIGRATIONS;
use serde::Deserialize;
use serde_json::Value;
use std::cell::Cell;
use worker::wasm_bindgen::closure::Closure;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{durable_object, js_sys, DurableObject, Env, Method, Request, Response, Result, SqlStorage, SqlStorageValue, State};

pub const TENANT_HEADER: &str = "X-Tenant-Id";
pub const ACTOR_HEADER: &str = "X-Actor-User-Id";

// ------------------------------------------------------------------ Db sobre o SQLite do DO

pub struct DoDb {
    sql: SqlStorage,
    /// objeto JS `ctx.storage`, para chamar `transactionSync` (o crate não expõe)
    storage: JsValue,
    depth: Cell<u32>,
}

fn js_message(v: &JsValue) -> String {
    if let Some(e) = v.dyn_ref::<js_sys::Error>() {
        return e.message().into();
    }
    v.as_string().unwrap_or_else(|| format!("{v:?}"))
}

fn to_sql(v: &Value) -> SqlStorageValue {
    match v {
        Value::Null => SqlStorageValue::Null,
        // o SQLite não tem booleano; o CHECK das colunas espera 0/1
        Value::Bool(b) => SqlStorageValue::Integer(*b as i64),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlStorageValue::Integer(i),
            None => SqlStorageValue::Float(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => SqlStorageValue::String(s.clone()),
        other => SqlStorageValue::String(other.to_string()),
    }
}

impl Db for DoDb {
    fn query(&self, sql: &str, params: &[Value]) -> crm_core::Result<Vec<Row>> {
        let binds: Vec<SqlStorageValue> = params.iter().map(to_sql).collect();
        self.sql
            .exec(sql, binds)
            .and_then(|cursor| cursor.to_array::<Row>())
            .map_err(|e| CoreError::from_sqlite_message(&e.to_string()))
    }

    fn batch(&self, sql: &str) -> crm_core::Result<()> {
        self.sql
            .exec(sql, None)
            .and_then(|cursor| cursor.to_array::<Value>())
            .map(|_| ())
            .map_err(|e| CoreError::from_sqlite_message(&e.to_string()))
    }

    /// `ctx.storage.transactionSync(cb)`: se `cb` lançar, o runtime desfaz tudo que
    /// ela escreveu. Um erro do core vira exceção JS pra disparar esse rollback.
    /// Aninhado, roda direto: o erro sobe e a transação de fora desfaz tudo.
    fn atomic<T>(&self, f: impl FnOnce() -> crm_core::Result<T>) -> crm_core::Result<T> {
        if self.depth.get() > 0 {
            return f();
        }
        let func: js_sys::Function = js_sys::Reflect::get(&self.storage, &JsValue::from_str("transactionSync"))
            .ok()
            .and_then(|v| v.dyn_into().ok())
            .ok_or_else(|| CoreError::Db("ctx.storage.transactionSync indisponível".into()))?;

        let mut slot: Option<crm_core::Result<T>> = None;
        let mut f = Some(f);
        let depth = &self.depth;
        let called = {
            let mut run = || -> std::result::Result<JsValue, JsValue> {
                depth.set(depth.get() + 1);
                let r = (f.take().expect("transactionSync chama o callback uma vez"))();
                depth.set(depth.get() - 1);
                let failed = r.is_err();
                slot = Some(r);
                if failed {
                    Err(JsValue::from_str("crm-core: rollback"))
                } else {
                    Ok(JsValue::UNDEFINED)
                }
            };
            let boxed: Box<dyn FnMut() -> std::result::Result<JsValue, JsValue> + '_> = Box::new(&mut run);
            // SEGURANÇA: transactionSync é síncrono e não guarda o callback. O Closure é
            // descartado logo abaixo, antes de `run` (e o que ele empresta) sair de escopo.
            let boxed: Box<dyn FnMut() -> std::result::Result<JsValue, JsValue> + 'static> = unsafe { std::mem::transmute(boxed) };
            let closure = Closure::wrap(boxed);
            let r = func.call1(&self.storage, closure.as_ref().unchecked_ref());
            drop(closure);
            r
        };
        match (called, slot) {
            (_, Some(Err(e))) => Err(e),
            (Ok(_), Some(Ok(v))) => Ok(v),
            (Err(js), _) => Err(CoreError::from_sqlite_message(&js_message(&js))),
            (Ok(_), None) => Err(CoreError::Db("transação não executou o callback".into())),
        }
    }
}

// ------------------------------------------------------------------ Durable Object

#[durable_object]
pub struct TenantDo {
    db: DoDb,
    /// Erro de migration no construtor. Se houver, toda requisição responde 500
    /// em vez de operar num banco pela metade.
    init: std::result::Result<(), String>,
}

impl DurableObject for TenantDo {
    fn new(state: State, _env: Env) -> Self {
        let raw = state._inner();
        let storage: JsValue = raw.storage().map(Into::into).unwrap_or(JsValue::NULL);
        let state: State = raw.into();
        let db = DoDb { sql: state.storage().sql(), storage, depth: Cell::new(0) };
        // SQL do DO é síncrono: roda antes de qualquer requisição ser entregue.
        let init = migrate(&db, TENANT_MIGRATIONS, now_ms()).map(|_| ()).map_err(|e| e.to_string());
        TenantDo { db, init }
    }

    async fn fetch(&self, req: Request) -> Result<Response> {
        if let Err(e) = &self.init {
            worker::console_error!("migration do tenant falhou: {e}");
            return fail(500, "tenant_init_failed", "banco do tenant não inicializou");
        }
        let Some(tenant) = req.headers().get(TENANT_HEADER)? else {
            return fail(400, "tenant_required", "requisição interna sem tenant");
        };
        if let Err(e) = ensure_tenant(&self.db, &tenant) {
            return from_core(&e);
        }
        let actor = req.headers().get(ACTOR_HEADER)?;
        let now = now_ms();
        let ids = move || uuid_v7(now);
        let ctx = Ctx::new(&self.db, now, actor, &ids);
        route(&ctx, req).await
    }
}

fn reply<T: serde::Serialize>(r: crm_core::Result<T>, status: u16) -> Result<Response> {
    match r {
        Ok(v) => ok(&v, status),
        Err(e) => from_core(&e),
    }
}

macro_rules! body {
    ($req:expr) => {
        match json_body(&mut $req).await {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        }
    };
}

#[derive(Deserialize)]
struct Note {
    body: String,
}

async fn route(ctx: &Ctx<'_, DoDb>, mut req: Request) -> Result<Response> {
    let url = req.url()?;
    let q = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
    let limit = q("limit").and_then(|v| v.parse::<u32>().ok()).unwrap_or(50);
    let path = url.path().trim_start_matches("/api/v1/").trim_end_matches('/').to_string();
    let seg: Vec<&str> = path.split('/').collect();

    match (req.method(), seg.as_slice()) {
        (Method::Get, ["contacts"]) => reply(contacts::list(ctx, limit, q("cursor").as_deref(), q("q").as_deref()), 200),
        (Method::Post, ["contacts"]) => {
            let input: NewContact = body!(req);
            reply(contacts::create(ctx, input), 201)
        }
        (Method::Get, ["contacts", id]) => reply(contacts::get(ctx, id), 200),
        (Method::Patch, ["contacts", id]) => {
            let input: ContactPatch = body!(req);
            reply(contacts::update(ctx, id, input), 200)
        }

        (Method::Get, ["pipelines"]) => reply(pipelines::list(ctx), 200),
        (Method::Post, ["pipelines"]) => {
            let input: NewPipeline = body!(req);
            reply(pipelines::create(ctx, input), 201)
        }
        (Method::Get, ["pipelines", id]) => reply(pipelines::get(ctx, id), 200),
        (Method::Post, ["pipelines", id, "stages"]) => {
            let input: NewStage = body!(req);
            reply(pipelines::add_stage(ctx, id, input), 201)
        }
        (Method::Get, ["pipelines", id, "board"]) => reply(leads::board(ctx, id), 200),

        (Method::Post, ["leads"]) => {
            let input: NewLead = body!(req);
            reply(leads::create(ctx, input), 201)
        }
        (Method::Get, ["leads", id]) => reply(leads::get(ctx, id), 200),
        (Method::Patch, ["leads", id]) => {
            let input: LeadPatch = body!(req);
            reply(leads::update(ctx, id, input), 200)
        }
        (Method::Post, ["leads", id, "move"]) => {
            let input: MoveLead = body!(req);
            reply(leads::move_lead(ctx, id, input), 200)
        }
        (Method::Post, ["leads", id, "notes"]) => {
            let input: Note = body!(req);
            reply(leads::add_note(ctx, id, &input.body).and_then(|_| activities::list(ctx, id, 1)), 201)
        }
        (Method::Get, ["leads", id, "activities"]) => reply(activities::list(ctx, id, limit), 200),

        _ => fail(404, "route_not_found", "rota não existe"),
    }
}
