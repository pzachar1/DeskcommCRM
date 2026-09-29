//! Quem fala com o modelo. É a única parte do agente que faz rede, e roda no
//! Alarm do DO — o mesmo lugar de onde sai o envio ao Kapso, e por isso fora de
//! qualquer transação.
//!
//! Dois provedores, escolhidos por tenant em `agent_config.provider`:
//!
//! | provider | rota | chave |
//! |---|---|---|
//! | `anthropic`  | `POST {base}/v1/messages` (Messages API) | `x-api-key: ANTHROPIC_API_KEY` |
//! | `workers_ai` | `POST {base}/{modelo}` (`/ai/run`)        | `Authorization: Bearer WORKERS_AI_TOKEN` |
//!
//! ## Como a base é resolvida (e por que nesta ordem)
//!
//! 1. `ANTHROPIC_API_BASE` / `WORKERS_AI_API_BASE`, quando presentes, mandam.
//!    Existem para o teste local apontar para um servidor falso — e servem
//!    também a quem põe um proxy próprio na frente.
//! 2. Com `AI_GATEWAY_ACCOUNT_ID` + `AI_GATEWAY_NAME`, vai pelo **AI Gateway** da
//!    Cloudflare (`https://gateway.ai.cloudflare.com/v1/{conta}/{gateway}/…`),
//!    que é onde ficam o cache, o log e o limite de gasto. É o caminho
//!    recomendado: o mesmo segredo, sem código a mais.
//! 3. Sem gateway, fala direto com o provedor (`api.anthropic.com`, ou
//!    `api.cloudflare.com/.../ai/run` com `CF_ACCOUNT_ID`).
//!
//! Falta de chave não é erro de rede: vira recusa PERMANENTE com código
//! `model_not_configured`, e a conversa vai para humano em vez de ficar tentando.

use crm_core::agent::{Generated, ReplyJob};
use crm_schema::model::AgentProvider;
use serde_json::{json, Value};
use worker::wasm_bindgen::JsValue;
use worker::{console_error, AbortSignal, Env, Fetch, Headers, Method, Request, RequestInit};

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const GATEWAY_BASE: &str = "https://gateway.ai.cloudflare.com/v1";
const MODEL_TIMEOUT_MS: u32 = 30_000;

/// Endereços e chaves dos provedores, lidos uma vez por rodada do Alarm.
pub struct Providers {
    anthropic_base: String,
    anthropic_key: Option<String>,
    workers_ai_base: Option<String>,
    workers_ai_token: Option<String>,
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name).ok().map(|v| v.to_string()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn secret(env: &Env, name: &str) -> Option<String> {
    env.secret(name).ok().map(|v| v.to_string()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Providers {
    pub fn from_env(env: &Env) -> Self {
        let gateway = var(env, "AI_GATEWAY_ACCOUNT_ID")
            .zip(var(env, "AI_GATEWAY_NAME"))
            .map(|(conta, nome)| format!("{GATEWAY_BASE}/{conta}/{nome}"));
        let anthropic_base = var(env, "ANTHROPIC_API_BASE")
            .or_else(|| gateway.as_ref().map(|g| format!("{g}/anthropic")))
            .unwrap_or_else(|| ANTHROPIC_BASE.to_string());
        let workers_ai_base = var(env, "WORKERS_AI_API_BASE")
            .or_else(|| gateway.as_ref().map(|g| format!("{g}/workers-ai")))
            .or_else(|| var(env, "CF_ACCOUNT_ID").map(|c| format!("https://api.cloudflare.com/client/v4/accounts/{c}/ai/run")));
        Providers {
            anthropic_base: anthropic_base.trim_end_matches('/').to_string(),
            anthropic_key: secret(env, "ANTHROPIC_API_KEY"),
            workers_ai_base: workers_ai_base.map(|b| b.trim_end_matches('/').to_string()),
            workers_ai_token: secret(env, "WORKERS_AI_TOKEN"),
        }
    }
}

fn nao_configurado(o_que: &str) -> Generated {
    Generated::Failed {
        code: "model_not_configured".into(),
        message: format!("falta {o_que} no Worker"),
        retryable: false,
    }
}

/// Manda a conversa ao modelo e devolve o que ele respondeu, já classificado.
pub async fn generate(p: &Providers, job: &ReplyJob) -> Generated {
    let inicio = crate::ids::now_ms();
    let pedido = match job.provider {
        AgentProvider::Anthropic => match &p.anthropic_key {
            Some(key) => {
                let headers = [("x-api-key", key.clone()), ("anthropic-version", ANTHROPIC_VERSION.to_string())];
                Some((format!("{}/v1/messages", p.anthropic_base), headers.to_vec(), anthropic_body(job)))
            }
            None => None,
        },
        AgentProvider::WorkersAi => match (&p.workers_ai_base, &p.workers_ai_token) {
            (Some(base), Some(token)) => Some((
                format!("{base}/{}", job.model),
                vec![("Authorization", format!("Bearer {token}"))],
                workers_ai_body(job),
            )),
            _ => None,
        },
    };
    let Some((url, headers, body)) = pedido else {
        return match job.provider {
            AgentProvider::Anthropic => nao_configurado("o segredo ANTHROPIC_API_KEY"),
            AgentProvider::WorkersAi => nao_configurado("WORKERS_AI_TOKEN (e a conta: CF_ACCOUNT_ID ou AI_GATEWAY_*)"),
        };
    };

    let (status, resposta) = match post(&url, &headers, &body).await {
        Ok(r) => r,
        Err(e) => {
            console_error!("modelo não respondeu ({}): {e}", job.conversation_id);
            (0, Value::Null)
        }
    };
    let latency_ms = Some(crate::ids::now_ms() - inicio);
    if !(200..300).contains(&status) {
        return classificar_erro(status, &resposta);
    }
    let lido = match job.provider {
        AgentProvider::Anthropic => ler_anthropic(&resposta),
        AgentProvider::WorkersAi => ler_workers_ai(&resposta),
    };
    match lido {
        Some((text, tokens_in, tokens_out)) => Generated::Reply { text, tokens_in, tokens_out, latency_ms },
        None => Generated::Failed {
            code: "invalid_model_response".into(),
            message: "o provedor respondeu 200 sem texto".into(),
            retryable: false,
        },
    }
}

async fn post(url: &str, headers: &[(&str, String)], body: &Value) -> worker::Result<(u16, Value)> {
    let h = Headers::new();
    h.set("Content-Type", "application/json")?;
    for (k, v) in headers {
        h.set(k, v)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post).with_headers(h).with_body(Some(JsValue::from_str(&body.to_string())));
    let req = Request::new_with_init(url, &init)?;
    let signal = AbortSignal::from(worker::web_sys::AbortSignal::timeout_with_u32(MODEL_TIMEOUT_MS));
    let mut resp = Fetch::Request(req).send_with_signal(&signal).await?;
    let status = resp.status_code();
    let texto = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str::<Value>(&texto).unwrap_or(Value::Null)))
}

/// Corpo da Messages API. O prompt do tenant vai em `system`, e o histórico em
/// `messages` — com as falas do mesmo lado fundidas, porque a API recusa dois
/// turnos seguidos do mesmo papel.
fn anthropic_body(job: &ReplyJob) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    for t in fundir(&job.turns) {
        msgs.push(json!({ "role": t.0, "content": t.1 }));
    }
    let mut body = json!({
        "model": job.model,
        "max_tokens": job.max_output_tokens,
        "temperature": job.temperature,
        "messages": msgs,
    });
    if !job.system_prompt.trim().is_empty() {
        body["system"] = json!(job.system_prompt);
    }
    body
}

/// Corpo do `/ai/run`: o prompt do tenant entra como turno `system`.
fn workers_ai_body(job: &ReplyJob) -> Value {
    let mut msgs: Vec<Value> = Vec::new();
    if !job.system_prompt.trim().is_empty() {
        msgs.push(json!({ "role": "system", "content": job.system_prompt }));
    }
    for t in fundir(&job.turns) {
        msgs.push(json!({ "role": t.0, "content": t.1 }));
    }
    json!({
        "messages": msgs,
        "max_tokens": job.max_output_tokens,
        "temperature": job.temperature,
    })
}

/// Junta falas seguidas do mesmo papel numa só, e garante que a conversa começa
/// pelo contato: a Messages API recusa turnos repetidos e histórico que abre no
/// `assistant` (acontece sempre que a empresa falou primeiro).
fn fundir(turns: &[crm_core::agent::Turn]) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    for t in turns.iter().skip_while(|t| t.role != "user") {
        match out.last_mut() {
            Some(ultimo) if ultimo.0 == t.role => {
                ultimo.1.push('\n');
                ultimo.1.push_str(&t.text);
            }
            _ => out.push((t.role, t.text.clone())),
        }
    }
    // Histórico curto que só pegou saída nossa (o `history_limit` cortou antes da
    // fala do contato) deixaria a lista vazia, e pedido sem mensagem é 400.
    if out.is_empty() {
        if let Some(ultimo) = turns.last() {
            out.push(("user", ultimo.text.clone()));
        }
    }
    out
}

fn ler_anthropic(v: &Value) -> Option<(String, Option<i64>, Option<i64>)> {
    let texto: String = v
        .get("content")?
        .as_array()?
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let uso = v.get("usage");
    Some((
        texto,
        uso.and_then(|u| u.get("input_tokens")).and_then(Value::as_i64),
        uso.and_then(|u| u.get("output_tokens")).and_then(Value::as_i64),
    ))
}

fn ler_workers_ai(v: &Value) -> Option<(String, Option<i64>, Option<i64>)> {
    let r = v.get("result")?;
    let texto = r
        .get("response")
        .and_then(Value::as_str)
        .or_else(|| {
            r.get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
        })?
        .to_string();
    let uso = r.get("usage");
    Some((
        texto,
        uso.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_i64),
        uso.and_then(|u| u.get("completion_tokens")).and_then(Value::as_i64),
    ))
}

/// `status = 0` é falha de rede (sem resposta). Sobrecarga e limite de taxa
/// passam sozinhos; pedido inválido e chave recusada, não.
fn classificar_erro(status: u16, corpo: &Value) -> Generated {
    let erro = corpo.get("error");
    let tipo = erro.and_then(|e| e.get("type")).and_then(Value::as_str).unwrap_or("");
    let message = erro
        .and_then(|e| e.get("message").or_else(|| e.get("error")))
        .and_then(Value::as_str)
        .map(str::to_string)
        // o /ai/run devolve `errors: [{ code, message }]`
        .or_else(|| {
            corpo
                .get("errors")
                .and_then(|e| e.get(0))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if status == 0 {
                "sem resposta do provedor".into()
            } else {
                format!("o provedor respondeu HTTP {status}")
            }
        });
    let retryable = status == 0
        || status == 408
        || status == 409
        || status == 429
        || status >= 500
        || matches!(tipo, "overloaded_error" | "rate_limit_error" | "api_error");
    let code = if tipo.is_empty() {
        if status == 0 {
            "network_error".to_string()
        } else {
            format!("http_{status}")
        }
    } else {
        tipo.to_string()
    };
    Generated::Failed { code, message, retryable }
}
