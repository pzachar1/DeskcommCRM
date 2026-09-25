/// Erro de domínio. Cada variante sabe o próprio status HTTP e código canônico,
/// então o Worker só serializa `{ error: { code, message } }`.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{0} não encontrado")]
    NotFound(&'static str),
    #[error("{message}")]
    Validation { code: &'static str, message: String },
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    /// Ainda não dá pra processar; tentar de novo mais tarde (503).
    #[error("{message}")]
    Retry { code: &'static str, message: String },
    #[error("erro de banco: {0}")]
    Db(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl CoreError {
    pub fn validation(code: &'static str, message: impl Into<String>) -> Self {
        CoreError::Validation { code, message: message.into() }
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        CoreError::Conflict { code, message: message.into() }
    }

    pub fn retry(code: &'static str, message: impl Into<String>) -> Self {
        CoreError::Retry { code, message: message.into() }
    }

    pub fn status(&self) -> u16 {
        match self {
            CoreError::NotFound(_) => 404,
            CoreError::Validation { .. } => 422,
            CoreError::Conflict { .. } => 409,
            CoreError::Retry { .. } => 503,
            CoreError::Db(_) => 500,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            CoreError::NotFound(_) => "not_found",
            CoreError::Validation { code, .. } | CoreError::Conflict { code, .. } | CoreError::Retry { code, .. } => code,
            CoreError::Db(_) => "internal_error",
        }
    }

    /// Traduz a mensagem de erro do SQLite (igual no rusqlite e no DO) para um erro
    /// de domínio. O que não for violação de constraint vira `Db`, que é 500.
    pub fn from_sqlite_message(msg: &str) -> Self {
        if let Some(rest) = msg.split("UNIQUE constraint failed: ").nth(1) {
            return CoreError::conflict("already_exists", format!("já existe registro com este valor ({})", rest.trim()));
        }
        if msg.contains("FOREIGN KEY constraint failed") {
            return CoreError::validation("invalid_reference", "referência para registro que não existe ou ainda está em uso");
        }
        if msg.contains("CHECK constraint failed") {
            return CoreError::validation("constraint_violation", "valor inválido para este campo");
        }
        CoreError::Db(msg.to_string())
    }
}
