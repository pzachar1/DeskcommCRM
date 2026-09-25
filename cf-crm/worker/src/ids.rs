use sha2::{Digest, Sha256};

pub fn now_ms() -> i64 {
    worker::Date::now().as_millis() as i64
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("crypto.getRandomValues indisponível");
    b
}

/// UUIDv7: ordena por tempo no B-tree do SQLite.
pub fn uuid_v7(now_ms: i64) -> String {
    uuid::Builder::from_unix_timestamp_millis(now_ms as u64, &random_bytes::<10>())
        .into_uuid()
        .to_string()
}

/// Token de sessão: 32 bytes aleatórios em hex. Vai no cookie; o banco guarda só o SHA-256.
pub fn session_token() -> String {
    hex::encode(random_bytes::<32>())
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
