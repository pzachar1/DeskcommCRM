//! `Db` sobre rusqlite, só para teste. Mesmo schema e mesmas mensagens de erro
//! do SQLite do Durable Object.
#![allow(dead_code)]

use crm_core::db::{Db, Row};
use crm_core::{CoreError, Ctx, Result};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::Connection;
use serde_json::{Number, Value};
use std::cell::Cell;
use std::rc::Rc;

pub struct SqliteDb {
    pub conn: Connection,
    depth: Cell<u32>,
}

impl SqliteDb {
    pub fn new() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        // igual ao workerd: FK sempre ligada
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        SqliteDb { conn, depth: Cell::new(0) }
    }
}

fn err(e: rusqlite::Error) -> CoreError {
    CoreError::from_sqlite_message(&e.to_string())
}

fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n.as_i64().map(SqlValue::Integer).unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap())),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

impl Db for SqliteDb {
    fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(sql).map_err(err)?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let params: Vec<SqlValue> = params.iter().map(to_sql).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params)).map_err(err)?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().map_err(err)? {
            let mut row = Row::new();
            for (i, n) in names.iter().enumerate() {
                let v = match r.get_ref(i).map_err(err)? {
                    ValueRef::Null => Value::Null,
                    ValueRef::Integer(i) => Value::Number(i.into()),
                    ValueRef::Real(f) => Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
                    ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                    ValueRef::Blob(_) => Value::Null,
                };
                row.insert(n.clone(), v);
            }
            out.push(row);
        }
        Ok(out)
    }

    fn batch(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql).map_err(err)
    }

    fn atomic<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let name = format!("sp{}", self.depth.get());
        self.depth.set(self.depth.get() + 1);
        self.conn.execute_batch(&format!("SAVEPOINT {name}")).map_err(err)?;
        let r = f();
        self.depth.set(self.depth.get() - 1);
        match r {
            Ok(v) => {
                self.conn.execute_batch(&format!("RELEASE {name}")).map_err(err)?;
                Ok(v)
            }
            Err(e) => {
                self.conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}")).map_err(err)?;
                Err(e)
            }
        }
    }
}

pub const NOW: i64 = 1_760_000_000_000;

/// Banco já migrado + gerador de id sequencial (ordena como UUIDv7).
pub struct Env {
    pub db: SqliteDb,
    ids: Box<dyn Fn() -> String>,
}

impl Env {
    pub fn new() -> Self {
        let db = SqliteDb::new();
        crm_core::migrate::migrate(&db, crm_schema::TENANT_MIGRATIONS, NOW).unwrap();
        crm_core::migrate::ensure_tenant(&db, "tenant-a").unwrap();
        let counter = Rc::new(Cell::new(0u64));
        let ids = Box::new(move || {
            counter.set(counter.get() + 1);
            format!("id-{:06}", counter.get())
        });
        Env { db, ids }
    }

    pub fn ctx(&self, now: i64) -> Ctx<'_, SqliteDb> {
        Ctx::new(&self.db, now, Some("user-1".to_string()), &*self.ids)
    }
}

#[macro_export]
macro_rules! ctx {
    ($env:expr) => {
        $env.ctx(common::NOW)
    };
    ($env:expr, $now:expr) => {
        $env.ctx($now)
    };
}
