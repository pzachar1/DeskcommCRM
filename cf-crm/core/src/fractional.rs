//! Fractional indexing para a ordem do kanban (algoritmo de David Greenspan,
//! "Implementing Fractional Indexing", o mesmo do pacote npm `fractional-indexing`).
//!
//! A chave tem uma parte inteira de tamanho variável (cabeça `a`-`z` para positivos,
//! `A`-`Z` para negativos) seguida de uma parte fracionária em base 62. Entre duas
//! chaves sempre cabe outra, então mover um card grava UMA linha. Colocar no início
//! ou no fim da coluna mexe só na parte inteira: a chave cresce em ordem logarítmica,
//! não linear. Ordena com `ORDER BY position` puro (ordem ASCII).

use crate::error::{CoreError, Result};

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: usize = 62;
const INTEGER_ZERO: &str = "a0";
const SMALLEST_INTEGER: &str = "A00000000000000000000000000";

fn bad(key: &str) -> CoreError {
    CoreError::validation("invalid_position", format!("chave de posição inválida: {key:?}"))
}

fn digit(c: u8) -> Option<usize> {
    DIGITS.iter().position(|&d| d == c)
}

fn integer_len(head: u8) -> Option<usize> {
    match head {
        b'a'..=b'z' => Some((head - b'a') as usize + 2),
        b'A'..=b'Z' => Some((b'Z' - head) as usize + 2),
        _ => None,
    }
}

fn integer_part(key: &str) -> Result<&str> {
    let len = key.bytes().next().and_then(integer_len).ok_or_else(|| bad(key))?;
    key.get(..len).ok_or_else(|| bad(key))
}

fn validate(key: &str) -> Result<()> {
    if key == SMALLEST_INTEGER || !key.bytes().all(|c| digit(c).is_some()) {
        return Err(bad(key));
    }
    let int = integer_part(key)?;
    if key[int.len()..].ends_with('0') {
        return Err(bad(key));
    }
    Ok(())
}

fn increment_integer(x: &str) -> Option<String> {
    let head = x.as_bytes()[0];
    let mut digs: Vec<u8> = x.as_bytes()[1..].to_vec();
    let mut carry = true;
    for d in digs.iter_mut().rev() {
        let next = digit(*d).expect("validado") + 1;
        if next == BASE {
            *d = b'0';
        } else {
            *d = DIGITS[next];
            carry = false;
            break;
        }
    }
    if carry {
        if head == b'Z' {
            return Some(INTEGER_ZERO.into());
        }
        if head == b'z' {
            return None;
        }
        let h = head + 1;
        if h > b'a' {
            digs.push(b'0');
        } else {
            digs.pop();
        }
        let mut out = vec![h];
        out.extend(digs);
        return Some(String::from_utf8(out).unwrap());
    }
    let mut out = vec![head];
    out.extend(digs);
    Some(String::from_utf8(out).unwrap())
}

fn decrement_integer(x: &str) -> Option<String> {
    let head = x.as_bytes()[0];
    let mut digs: Vec<u8> = x.as_bytes()[1..].to_vec();
    let mut borrow = true;
    for d in digs.iter_mut().rev() {
        let v = digit(*d).expect("validado");
        if v == 0 {
            *d = DIGITS[BASE - 1];
        } else {
            *d = DIGITS[v - 1];
            borrow = false;
            break;
        }
    }
    if borrow {
        if head == b'a' {
            return Some(format!("Z{}", DIGITS[BASE - 1] as char));
        }
        if head == b'A' {
            return None;
        }
        let h = head - 1;
        if h < b'Z' {
            digs.push(DIGITS[BASE - 1]);
        } else {
            digs.pop();
        }
        let mut out = vec![h];
        out.extend(digs);
        return Some(String::from_utf8(out).unwrap());
    }
    let mut out = vec![head];
    out.extend(digs);
    Some(String::from_utf8(out).unwrap())
}

/// Ponto médio da parte fracionária entre `a` (vazio = 0) e `b` (None = 1). Pré-condição: a < b.
fn midpoint(a: &[u8], b: Option<&[u8]>) -> Vec<u8> {
    if let Some(b) = b {
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(b'0') == b[n] {
            n += 1;
        }
        if n > 0 {
            let mut out = b[..n].to_vec();
            out.extend(midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..])));
            return out;
        }
    }
    let da = a.first().map(|&c| digit(c).unwrap()).unwrap_or(0);
    let db = b.map(|b| digit(b[0]).unwrap()).unwrap_or(BASE);
    if db - da > 1 {
        vec![DIGITS[(da + db) / 2]]
    } else if let Some(b) = b.filter(|b| b.len() > 1) {
        vec![b[0]]
    } else {
        let mut out = vec![DIGITS[da]];
        out.extend(midpoint(a.get(1..).unwrap_or(&[]), None));
        out
    }
}

fn join(int: &str, frac: Vec<u8>) -> String {
    format!("{int}{}", String::from_utf8(frac).expect("só dígitos ASCII"))
}

/// Chave entre `before` e `after`. `None` nas duas pontas = primeira chave (`a0`).
pub fn key_between(before: Option<&str>, after: Option<&str>) -> Result<String> {
    if let Some(a) = before {
        validate(a)?;
    }
    if let Some(b) = after {
        validate(b)?;
    }
    let exhausted = || CoreError::validation("position_exhausted", "sem espaço de posição nesta ponta");
    match (before, after) {
        (Some(a), Some(b)) if a >= b => Err(CoreError::validation("invalid_position", format!("{a:?} não vem antes de {b:?}"))),
        (None, None) => Ok(INTEGER_ZERO.into()),
        (None, Some(b)) => {
            let ib = integer_part(b)?;
            let fb = &b[ib.len()..];
            if ib == SMALLEST_INTEGER {
                return Ok(join(ib, midpoint(b"", Some(fb.as_bytes()))));
            }
            if ib.len() < b.len() {
                return Ok(ib.to_string());
            }
            decrement_integer(ib).ok_or_else(exhausted)
        }
        (Some(a), None) => {
            let ia = integer_part(a)?;
            let fa = &a[ia.len()..];
            Ok(increment_integer(ia).unwrap_or_else(|| join(ia, midpoint(fa.as_bytes(), None))))
        }
        (Some(a), Some(b)) => {
            let ia = integer_part(a)?;
            let fa = &a[ia.len()..];
            let ib = integer_part(b)?;
            let fb = &b[ib.len()..];
            if ia == ib {
                return Ok(join(ia, midpoint(fa.as_bytes(), Some(fb.as_bytes()))));
            }
            let i = increment_integer(ia).ok_or_else(exhausted)?;
            if i.as_str() < b {
                return Ok(i);
            }
            Ok(join(ia, midpoint(fa.as_bytes(), None)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key_and_ends() {
        let k = key_between(None, None).unwrap();
        assert_eq!(k, "a0");
        assert_eq!(key_between(Some("a0"), None).unwrap(), "a1");
        assert_eq!(key_between(None, Some("a0")).unwrap(), "Zz");
        assert_eq!(key_between(Some("a0"), Some("a1")).unwrap(), "a0V");
        assert!(key_between(None, Some(&k)).unwrap() < k);
        assert!(key_between(Some(&k), None).unwrap() > k);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(key_between(Some("b"), Some("a")).is_err());
        assert!(key_between(Some("a"), Some("a")).is_err());
        assert!(key_between(Some("a00"), None).is_err(), "fração terminando em 0");
        assert!(key_between(Some("a-"), None).is_err());
        assert!(key_between(Some("b1"), None).is_err(), "parte inteira curta demais");
    }

    /// Insere sempre no mesmo vão (o pior caso do kanban: arrastar pro topo
    /// repetidas vezes) e em posições pseudoaleatórias. A ordem nunca quebra.
    #[test]
    fn order_survives_thousands_of_inserts() {
        let mut keys: Vec<String> = vec![key_between(None, None).unwrap()];
        for _ in 0..500 {
            let k = key_between(None, Some(&keys[0])).unwrap();
            keys.insert(0, k);
        }
        for _ in 0..500 {
            let k = key_between(Some(&keys[0]), Some(&keys[1])).unwrap();
            keys.insert(1, k);
        }
        let mut seed: u64 = 42;
        for _ in 0..3000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let i = (seed >> 33) as usize % (keys.len() + 1);
            let before = if i == 0 { None } else { Some(keys[i - 1].as_str()) };
            let after = keys.get(i).map(String::as_str);
            let k = key_between(before, after).unwrap();
            keys.insert(i, k);
        }
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "{} >= {}", w[0], w[1]);
        }
        assert!(keys.iter().all(|k| validate(k).is_ok()), "toda chave gerada é válida");
        let longest = keys.iter().map(String::len).max().unwrap();
        assert!(longest < 120, "chave cresceu demais: {longest}");
    }

    /// O caso comum do CRM: lead novo sempre no fim da coluna.
    #[test]
    fn appending_stays_short() {
        let mut last = key_between(None, None).unwrap();
        for _ in 0..10_000 {
            let next = key_between(Some(&last), None).unwrap();
            assert!(next > last);
            last = next;
        }
        assert!(last.len() <= 4, "10 mil no fim da coluna e a chave tem {} chars: {last}", last.len());
    }
}
