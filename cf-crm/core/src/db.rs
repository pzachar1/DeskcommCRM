//! Porta de banco. O core fala só com este trait; o DO (via `sql.exec`) e os testes
//! (via rusqlite) implementam. Parâmetros e linhas são `serde_json::Value` porque é o
//! denominador comum dos dois lados.

use crate::error::{CoreError, Result};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

pub type Row = Map<String, Value>;

pub trait Db {
    /// Uma instrução, com parâmetros posicionais `?`.
    fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>>;

    /// Várias instruções separadas por `;`, sem parâmetros (migrations).
    fn batch(&self, sql: &str) -> Result<()>;

    /// Tudo ou nada: se `f` devolver erro, nada do que ela escreveu fica.
    fn atomic<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T>;
}

pub fn exec(db: &impl Db, sql: &str, params: &[Value]) -> Result<()> {
    db.query(sql, params).map(|_| ())
}

pub fn all<T: DeserializeOwned>(db: &impl Db, sql: &str, params: &[Value]) -> Result<Vec<T>> {
    db.query(sql, params)?.into_iter().map(from_row).collect()
}

pub fn one<T: DeserializeOwned>(db: &impl Db, sql: &str, params: &[Value]) -> Result<Option<T>> {
    db.query(sql, params)?.into_iter().next().map(from_row).transpose()
}

pub fn from_row<T: DeserializeOwned>(row: Row) -> Result<T> {
    serde_json::from_value(Value::Object(row)).map_err(|e| CoreError::Db(format!("linha fora do formato: {e}")))
}

/// Coluna JSON do SQLite é TEXT: grava o valor serializado.
pub fn json_param(v: &Value) -> Value {
    Value::String(v.to_string())
}
