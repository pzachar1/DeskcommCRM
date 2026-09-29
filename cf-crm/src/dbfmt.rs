//! Leitura de linha do SQLite para os structs do modelo.
//!
//! O SQLite devolve booleano como 0/1 e JSON como texto. Estes deserializadores
//! aceitam os dois formatos (o do banco e o nativo), então o mesmo struct serve
//! para ler do banco e para receber JSON de uma API.

use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::fmt;

pub fn bool_int<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = bool;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("booleano ou 0/1")
        }
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<bool, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<bool, E> {
            match v {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(E::custom(format!("booleano fora de 0/1: {v}"))),
            }
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<bool, E> {
            self.visit_i64(v as i64)
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<bool, E> {
            self.visit_i64(v as i64)
        }
    }
    d.deserialize_any(V)
}

fn parse_json<E: de::Error>(v: Value) -> Result<Value, E> {
    match v {
        Value::String(s) => serde_json::from_str(&s).map_err(E::custom),
        other => Ok(other),
    }
}

pub fn json_text<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    parse_json(Value::deserialize(d)?)
}

pub fn opt_json_text<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    match Option::<Value>::deserialize(d)? {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse_json(v).map(Some),
    }
}
