//! O AGENTE RESPONDE SOZINHO — a decisão, sem I/O.
//!
//! O que este módulo faz e o que ele não faz:
//!
//! - **decide** se a conversa merece resposta automática agora ([`after_inbound`]),
//! - **monta** o que vai ao modelo ([`claim_due`]: config + histórico da conversa),
//! - **grava** o que o modelo devolveu ([`complete`]).
//!
//! Quem fala com o modelo é o Worker (`worker/src/agent.rs`), chamado pelo Alarm
//! do DO — mesmo lugar de onde sai o envio ao Kapso. Nenhuma chamada de rede
//! acontece dentro de transação.
//!
//! ## A resposta do agente NÃO tem caminho próprio de envio
//!
//! Ela entra por [`crate::messaging::send_as`] com `sent_via = 'ai'`: mesma linha
//! em `messages`, mesma linha em `outbox`, mesma garantia de sair no máximo uma
//! vez. A chave de idempotência é `agent:<id da mensagem do contato>`, e é ela
//! que segura o pior caso do agente: o DO cair depois de gravar a resposta e
//! antes de apagar o job. Na rodada seguinte o `uq_messages_idempotency` devolve
//! a mesma mensagem, e nada sai duas vezes.
//!
//! ## Debounce: uma linha por conversa
//!
//! `agent_jobs` tem o `conversation_id` como chave primária. Quem escreve "oi",
//! "boa tarde", "queria saber o preço" em três mensagens recebe UMA resposta: a
//! segunda e a terceira empurram `run_after` em vez de criar outra rodada.
//!
//! ## Nada morre calado
//!
//! Toda rodada deixa linha em `agent_runs` — inclusive a que decidiu não
//! responder, com o motivo. Erro que esgota as tentativas não fica em silêncio:
//! a conversa vai para humano (`status = 'human'`), que é o único jeito de a
//! pessoa do outro lado não ficar esperando um robô que desistiu.
//!
//! A exceção deliberada é **agente desligado**: aí não há rodada nem linha de
//! log. Registrar "não respondi porque estou desligado" a cada mensagem enche a
//! tabela em toda instalação que não usa agente — o mesmo defeito da batida de
//! cron que audita sem ter feito nada.

use crate::db::{all, exec, one, Db};
use crate::messaging::{self, Outgoing};
use crate::{contacts, opt_out, CoreError, Ctx, Page, Result};
use crm_schema::model::{
    AgentConfig, AgentOutcomeKind, AgentProvider, AgentRun, Contact, Conversation, ConversationStatus, MessageType, SentVia,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Prefixo da chave de idempotência da resposta automática.
pub const REPLY_IDEMPOTENCY_PREFIX: &str = "agent:";
/// O modelo pede humano escrevendo isto na resposta. Sai do texto antes de enviar.
pub const HANDOFF_MARKER: &str = "[HUMANO]";
/// Rodada em andamento por mais que isto foi interrompida: pode tentar de novo.
/// Repetir a chamada ao modelo só custa token — a idempotência do envio garante
/// que a resposta não sai duas vezes.
pub const IN_FLIGHT_LEASE_MS: i64 = 2 * 60 * 1000;
/// Tentativas antes de desistir e chamar humano.
pub const MAX_ATTEMPTS: i64 = 4;
const RATE_WINDOW_MS: i64 = 60 * 60 * 1000;
/// Teto de texto do histórico que vai ao modelo, por mensagem.
const HISTORY_CHARS: usize = 1200;

// ------------------------------------------------------------------ config

pub fn config<D: Db>(ctx: &Ctx<D>) -> Result<AgentConfig> {
    Ok(one(ctx.db, "SELECT * FROM agent_config WHERE id = 'default'", &[])?.unwrap_or_default())
}

/// Corpo de `PUT /api/v1/agent`. Campo ausente não muda.
#[derive(Debug, Default, Deserialize)]
pub struct ConfigPatch {
    pub is_enabled: Option<bool>,
    pub provider: Option<AgentProvider>,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
    pub max_output_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub history_limit: Option<i64>,
    pub debounce_ms: Option<i64>,
    pub human_silence_ms: Option<i64>,
    pub max_replies_per_hour: Option<i64>,
    pub handoff_keywords: Option<Vec<String>>,
}

fn faixa(nome: &'static str, valor: i64, min: i64, max: i64) -> Result<i64> {
    if (min..=max).contains(&valor) {
        Ok(valor)
    } else {
        Err(CoreError::validation("invalid_agent_config", format!("{nome} tem de estar entre {min} e {max}")))
    }
}

/// Grava a config (uma linha, `id = 'default'`). Ligar o agente exige instrução:
/// um agente sem prompt responde em nome da empresa sem saber nada dela.
pub fn save_config<D: Db>(ctx: &Ctx<D>, patch: ConfigPatch) -> Result<AgentConfig> {
    ctx.db.atomic(|| {
        let atual = config(ctx)?;
        let mut c = AgentConfig {
            is_enabled: patch.is_enabled.unwrap_or(atual.is_enabled),
            provider: patch.provider.unwrap_or(atual.provider),
            model: patch.model.unwrap_or(atual.model).trim().to_string(),
            system_prompt: patch.system_prompt.unwrap_or(atual.system_prompt),
            max_output_tokens: faixa("max_output_tokens", patch.max_output_tokens.unwrap_or(atual.max_output_tokens), 16, 4096)?,
            temperature: patch.temperature.unwrap_or(atual.temperature),
            history_limit: faixa("history_limit", patch.history_limit.unwrap_or(atual.history_limit), 1, 100)?,
            debounce_ms: faixa("debounce_ms", patch.debounce_ms.unwrap_or(atual.debounce_ms), 0, 120_000)?,
            human_silence_ms: faixa("human_silence_ms", patch.human_silence_ms.unwrap_or(atual.human_silence_ms), 0, 604_800_000)?,
            max_replies_per_hour: faixa("max_replies_per_hour", patch.max_replies_per_hour.unwrap_or(atual.max_replies_per_hour), 1, 120)?,
            handoff_keywords: match patch.handoff_keywords {
                Some(v) => json!(v),
                None => atual.handoff_keywords,
            },
            updated_by: ctx.actor.clone(),
            updated_at: ctx.now_ms,
        };
        if c.model.is_empty() || c.model.chars().count() > 200 {
            return Err(CoreError::validation("invalid_agent_config", "informe o modelo (até 200 caracteres)"));
        }
        if !(0.0..=2.0).contains(&c.temperature) {
            return Err(CoreError::validation("invalid_agent_config", "temperature tem de estar entre 0 e 2"));
        }
        c.system_prompt = c.system_prompt.trim().to_string();
        if c.system_prompt.chars().count() > 20_000 {
            return Err(CoreError::validation("invalid_agent_config", "o prompt passa de 20.000 caracteres"));
        }
        if c.is_enabled && c.system_prompt.is_empty() {
            return Err(CoreError::validation(
                "agent_prompt_required",
                "escreva o que o agente deve fazer antes de ligá-lo: sem instrução, ele responde em nome da empresa sem saber nada dela",
            ));
        }
        exec(
            ctx.db,
            "INSERT INTO agent_config (id, is_enabled, provider, model, system_prompt, max_output_tokens, temperature,
                                       history_limit, debounce_ms, human_silence_ms, max_replies_per_hour, handoff_keywords,
                                       updated_by, updated_at)
             VALUES ('default', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
               is_enabled = excluded.is_enabled, provider = excluded.provider, model = excluded.model,
               system_prompt = excluded.system_prompt, max_output_tokens = excluded.max_output_tokens,
               temperature = excluded.temperature, history_limit = excluded.history_limit,
               debounce_ms = excluded.debounce_ms, human_silence_ms = excluded.human_silence_ms,
               max_replies_per_hour = excluded.max_replies_per_hour, handoff_keywords = excluded.handoff_keywords,
               updated_by = excluded.updated_by, updated_at = excluded.updated_at",
            &[
                json!(c.is_enabled),
                json!(c.provider.as_str()),
                json!(c.model),
                json!(c.system_prompt),
                json!(c.max_output_tokens),
                json!(c.temperature),
                json!(c.history_limit),
                json!(c.debounce_ms),
                json!(c.human_silence_ms),
                json!(c.max_replies_per_hour),
                json!(crate::db::json_param(&c.handoff_keywords)),
                json!(c.updated_by),
                json!(c.updated_at),
            ],
        )?;
        config(ctx)
    })
}

// ------------------------------------------------------------------ log

fn log_run<D: Db>(
    ctx: &Ctx<D>,
    conversation_id: &str,
    trigger: Option<&str>,
    reply: Option<&str>,
    outcome: AgentOutcomeKind,
    reason: Option<&str>,
    uso: Option<&Uso>,
) -> Result<()> {
    exec(
        ctx.db,
        "INSERT INTO agent_runs (id, conversation_id, trigger_message_id, reply_message_id, outcome, reason,
                                 provider, model, tokens_in, tokens_out, latency_ms, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            json!(ctx.new_id()),
            json!(conversation_id),
            json!(trigger),
            json!(reply),
            json!(outcome.as_str()),
            json!(reason),
            json!(uso.map(|u| u.provider.as_str())),
            json!(uso.map(|u| u.model.clone())),
            json!(uso.and_then(|u| u.tokens_in)),
            json!(uso.and_then(|u| u.tokens_out)),
            json!(uso.and_then(|u| u.latency_ms)),
            json!(ctx.now_ms),
        ],
    )
}

/// Provedor, modelo e custo de uma rodada — o que a tela mostra por linha de log.
#[derive(Debug, Clone)]
pub struct Uso {
    pub provider: AgentProvider,
    pub model: String,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub latency_ms: Option<i64>,
}

/// Rodadas mais recentes primeiro. Cursor `created_at.id`.
pub fn list_runs<D: Db>(ctx: &Ctx<D>, limit: u32, cursor: Option<&str>) -> Result<Page<AgentRun>> {
    let limit = limit.clamp(1, 200);
    let (c_at, c_id) = match cursor {
        Some(c) => {
            let (t, id) = c.split_once('.').ok_or_else(|| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            let t: i64 = t.parse().map_err(|_| CoreError::validation("invalid_cursor", "cursor inválido"))?;
            (json!(t), json!(id))
        }
        None => (Value::Null, Value::Null),
    };
    let mut items: Vec<AgentRun> = all(
        ctx.db,
        "SELECT * FROM agent_runs WHERE (?1 IS NULL OR (created_at, id) < (?1, ?2))
         ORDER BY created_at DESC, id DESC LIMIT ?3",
        &[c_at, c_id, json!(limit + 1)],
    )?;
    let next_cursor = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|r| format!("{}.{}", r.created_at, r.id))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

/// O contato pediu para sair (a ingestão acabou de bloqueá-lo). Rodada pendente
/// desta conversa é cancelada: uma resposta que já estava na fila não pode sair
/// depois do pedido. A linha de log só existe se o agente estiver ligado.
pub fn on_opt_out<D: Db>(ctx: &Ctx<D>, conversation_id: &str, message_id: &str) -> Result<()> {
    exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(conversation_id)])?;
    if !config(ctx)?.is_enabled {
        return Ok(());
    }
    log_run(ctx, conversation_id, Some(message_id), None, AgentOutcomeKind::Skipped, Some("opt_out"), None)
}

// ------------------------------------------------------------------ mão humana <-> agente

/// Passa a conversa para humano. Vale para o pedido do contato, o do modelo, o
/// clique de quem atende e a desistência depois de erro.
pub fn handoff<D: Db>(ctx: &Ctx<D>, conversation_id: &str, reason: &str) -> Result<Conversation> {
    ctx.db.atomic(|| {
        let conv = messaging::get_conversation(ctx, conversation_id)?;
        exec(
            ctx.db,
            "UPDATE conversations SET status = ?, status_changed_at = ?, last_handoff_at = ?, last_handoff_reason = ?, updated_at = ?
             WHERE id = ?",
            &[
                json!(ConversationStatus::Human.as_str()),
                json!(ctx.now_ms),
                json!(ctx.now_ms),
                json!(reason),
                json!(ctx.now_ms),
                json!(conv.id),
            ],
        )?;
        // Conversa em mão humana não tem rodada pendente.
        exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(conv.id)])?;
        messaging::get_conversation(ctx, conversation_id)
    })
}

/// Devolve a conversa ao agente: limpa o silêncio e volta a atender. É o outro
/// lado do handoff — sem isto, conversa que foi para humano nunca voltaria.
pub fn back_to_ai<D: Db>(ctx: &Ctx<D>, conversation_id: &str) -> Result<Conversation> {
    ctx.db.atomic(|| {
        let conv = messaging::get_conversation(ctx, conversation_id)?;
        let cfg = config(ctx)?;
        if !cfg.is_enabled {
            return Err(CoreError::conflict("agent_disabled", "o agente está desligado nas configurações"));
        }
        exec(
            ctx.db,
            "UPDATE conversations SET status = ?, status_changed_at = ?, bot_silenced_until = NULL, updated_at = ? WHERE id = ?",
            &[json!(ConversationStatus::AiHandling.as_str()), json!(ctx.now_ms), json!(ctx.now_ms), json!(conv.id)],
        )?;
        messaging::get_conversation(ctx, conversation_id)
    })
}

/// Um humano respondeu (pela tela ou pelo app do WhatsApp): o agente cala nesta
/// conversa pelo tempo configurado. Sem isto, o robô fala em cima do atendente.
pub fn silence_for_human<D: Db>(ctx: &Ctx<D>, conversation_id: &str) -> Result<()> {
    let cfg = config(ctx)?;
    if !cfg.is_enabled {
        return Ok(());
    }
    exec(
        ctx.db,
        "UPDATE conversations SET
            bot_silenced_until = MAX(COALESCE(bot_silenced_until, 0), ?1),
            status = CASE WHEN status IN ('open', 'ai_handling') THEN ?2 ELSE status END,
            status_changed_at = CASE WHEN status IN ('open', 'ai_handling') THEN ?3 ELSE status_changed_at END,
            updated_at = ?3
         WHERE id = ?4",
        &[
            json!(ctx.now_ms + cfg.human_silence_ms),
            json!(ConversationStatus::Human.as_str()),
            json!(ctx.now_ms),
            json!(conversation_id),
        ],
    )?;
    exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(conversation_id)])
}

// ------------------------------------------------------------------ decisão

/// O que a entrada de uma mensagem provocou. `Skipped` é decisão com motivo,
/// não erro.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "agent", rename_all = "snake_case")]
pub enum Scheduled {
    /// Resposta agendada para depois da espera (debounce).
    Queued { run_after: i64 },
    /// Conversa passada para humano agora.
    Handoff { reason: String },
    /// Nada a fazer, e por quê.
    Skipped { reason: String },
}

/// Por que o agente NÃO deve responder esta conversa agora. `None` = pode.
fn barreira(cfg: &AgentConfig, conv: &Conversation, contact: &Contact, now_ms: i64) -> Option<&'static str> {
    if !cfg.is_enabled {
        return Some("agent_disabled");
    }
    if contact.is_blocked {
        return Some("contact_blocked");
    }
    if contact.is_anonymized {
        return Some("contact_anonymized");
    }
    if contact.force_human {
        return Some("contact_force_human");
    }
    if conv.status == ConversationStatus::Human {
        return Some("human_handling");
    }
    if conv.bot_silenced_until.is_some_and(|t| t > now_ms) {
        return Some("bot_silenced");
    }
    if conv.snooze_until.is_some_and(|t| t > now_ms) {
        return Some("snoozed");
    }
    if !conv.service_window_open(now_ms) {
        return Some("service_window_closed");
    }
    None
}

/// Chamada pela ingestão para CADA mensagem nova do contato, dentro da mesma
/// transação. Decide entre agendar resposta, passar para humano e não fazer nada.
///
/// O pedido de saída ("pare de me mandar mensagem") é tratado antes, na
/// ingestão: quem chega aqui bloqueado bate em `contact_blocked`.
pub fn after_inbound<D: Db>(
    ctx: &Ctx<D>,
    conversation_id: &str,
    contact: &Contact,
    message_id: &str,
    kind: MessageType,
    text: Option<&str>,
) -> Result<Scheduled> {
    let cfg = config(ctx)?;
    let conv = messaging::get_conversation(ctx, conversation_id)?;
    if let Some(reason) = barreira(&cfg, &conv, contact, ctx.now_ms) {
        // Agente desligado não gera log: senão toda instalação que não usa
        // agente acumula uma linha por mensagem recebida.
        if cfg.is_enabled {
            log_run(ctx, conversation_id, Some(message_id), None, AgentOutcomeKind::Skipped, Some(reason), None)?;
        }
        return Ok(Scheduled::Skipped { reason: reason.to_string() });
    }

    let corpo = text.map(str::trim).filter(|t| !t.is_empty());

    // Sinal ambíguo de saída ("me deixa em paz"): não bloqueia ninguém, mas o
    // robô não insiste — quem confirma o descadastro é uma pessoa.
    if corpo.is_some_and(opt_out::opt_out_provavel) {
        handoff(ctx, conversation_id, "opt_out_provavel")?;
        log_run(ctx, conversation_id, Some(message_id), None, AgentOutcomeKind::Handoff, Some("opt_out_provavel"), None)?;
        return Ok(Scheduled::Handoff { reason: "opt_out_provavel".into() });
    }

    // Pedido explícito de atendente: passa direto, sem gastar modelo.
    if let Some(t) = corpo {
        let n = opt_out::normalizar(t);
        if cfg.handoff_phrases().iter().any(|f| n.contains(f)) {
            handoff(ctx, conversation_id, "handoff_keyword")?;
            log_run(ctx, conversation_id, Some(message_id), None, AgentOutcomeKind::Handoff, Some("handoff_keyword"), None)?;
            return Ok(Scheduled::Handoff { reason: "handoff_keyword".into() });
        }
    }

    // Só responde o que dá para ler. Foto, áudio sem transcrição e figurinha
    // continuam na caixa de entrada, com o contador de não lidas chamando gente.
    if corpo.is_none() {
        log_run(
            ctx,
            conversation_id,
            Some(message_id),
            None,
            AgentOutcomeKind::Skipped,
            Some("unsupported_inbound_type"),
            None,
        )?;
        return Ok(Scheduled::Skipped { reason: format!("sem texto para ler: {}", kind.as_str()) });
    }

    let run_after = ctx.now_ms + cfg.debounce_ms;
    exec(
        ctx.db,
        "INSERT INTO agent_jobs (conversation_id, trigger_message_id, run_after, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT (conversation_id) DO UPDATE SET
           trigger_message_id = excluded.trigger_message_id,
           run_after = excluded.run_after,
           updated_at = excluded.updated_at
         WHERE agent_jobs.in_flight_at IS NULL",
        &[json!(conversation_id), json!(message_id), json!(run_after), json!(ctx.now_ms)],
    )?;
    Ok(Scheduled::Queued { run_after })
}

// ------------------------------------------------------------------ rodada

/// Uma fala do histórico, no vocabulário que todo provedor entende.
#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    /// `user` = o contato; `assistant` = qualquer saída nossa (agente, atendente
    /// ou automação). Para o modelo, tudo que saiu daqui é a própria voz.
    pub role: &'static str,
    pub text: String,
}

/// Uma rodada pronta para ir ao modelo. O core não conhece URL nem chave.
#[derive(Debug, Clone)]
pub struct ReplyJob {
    pub conversation_id: String,
    pub trigger_message_id: String,
    pub attempts: i64,
    pub provider: AgentProvider,
    pub model: String,
    pub system_prompt: String,
    pub max_output_tokens: i64,
    pub temperature: f64,
    pub turns: Vec<Turn>,
}

/// O que o modelo devolveu, já classificado pelo Worker.
#[derive(Debug, Clone)]
pub enum Generated {
    Reply { text: String, tokens_in: Option<i64>, tokens_out: Option<i64>, latency_ms: Option<i64> },
    Failed { code: String, message: String, retryable: bool },
}

fn history<D: Db>(ctx: &Ctx<D>, conversation_id: &str, limit: i64) -> Result<Vec<Turn>> {
    #[derive(Deserialize)]
    struct Row {
        direction: String,
        #[serde(rename = "type")]
        kind: String,
        body: Option<String>,
    }
    // Mais recentes primeiro pela hora da Meta (mensagem atrasada cai no lugar
    // certo), depois invertidas: o modelo lê na ordem em que se falou.
    let rows: Vec<Row> = all(
        ctx.db,
        "SELECT direction, type, body FROM messages
         WHERE conversation_id = ? AND status <> 'failed'
         ORDER BY sent_at DESC, id DESC LIMIT ?",
        &[json!(conversation_id), json!(limit.clamp(1, 100))],
    )?;
    let mut turns: Vec<Turn> = rows
        .into_iter()
        .map(|r| {
            let role = if r.direction == "inbound" { "user" } else { "assistant" };
            let text = match r.body.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()) {
                Some(b) => b.chars().take(HISTORY_CHARS).collect(),
                None => format!("[{}]", r.kind),
            };
            Turn { role, text }
        })
        .collect();
    turns.reverse();
    Ok(turns)
}

fn replies_na_janela<D: Db>(ctx: &Ctx<D>, conversation_id: &str) -> Result<i64> {
    let row: Option<Value> = one(
        ctx.db,
        "SELECT COUNT(*) AS n FROM agent_runs WHERE conversation_id = ? AND outcome = 'replied' AND created_at > ?",
        &[json!(conversation_id), json!(ctx.now_ms - RATE_WINDOW_MS)],
    )?;
    Ok(row.and_then(|r| r.get("n").and_then(Value::as_i64)).unwrap_or(0))
}

/// Rodadas devidas, já com histórico e config. O que sair daqui PRECISA terminar
/// em [`complete`].
///
/// A barreira é conferida DE NOVO aqui, e não só no agendamento: durante a
/// espera do debounce um atendente pode ter entrado na conversa, e o agente que
/// fala em cima dele é pior do que o que demora.
pub fn claim_due<D: Db>(ctx: &Ctx<D>, limit: u32) -> Result<Vec<ReplyJob>> {
    ctx.db.atomic(|| {
        // Rodada interrompida (o DO caiu no meio da chamada): pode tentar de novo.
        exec(
            ctx.db,
            "UPDATE agent_jobs SET in_flight_at = NULL, updated_at = ?1, last_error = COALESCE(last_error, 'rodada interrompida')
             WHERE in_flight_at IS NOT NULL AND in_flight_at <= ?2",
            &[json!(ctx.now_ms), json!(ctx.now_ms - IN_FLIGHT_LEASE_MS)],
        )?;

        let cfg = config(ctx)?;
        let due: Vec<crm_schema::model::AgentJobRow> = all(
            ctx.db,
            "SELECT * FROM agent_jobs WHERE in_flight_at IS NULL AND run_after <= ? ORDER BY run_after LIMIT ?",
            &[json!(ctx.now_ms), json!(limit.clamp(1, 20))],
        )?;
        let mut jobs = Vec::with_capacity(due.len());
        for item in due {
            let conv = messaging::get_conversation(ctx, &item.conversation_id)?;
            let contact = contacts::get(ctx, &conv.contact_id)?;
            let mut recusa = barreira(&cfg, &conv, &contact, ctx.now_ms);
            if recusa.is_none() && replies_na_janela(ctx, &conv.id)? >= cfg.max_replies_per_hour {
                recusa = Some("rate_limited");
            }
            if let Some(reason) = recusa {
                // Teto de respostas estourado é sinal de laço (outro robô do
                // outro lado): não basta calar, alguém tem de olhar.
                if reason == "rate_limited" {
                    handoff(ctx, &conv.id, reason)?;
                }
                log_run(ctx, &conv.id, Some(&item.trigger_message_id), None, AgentOutcomeKind::Skipped, Some(reason), None)?;
                exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(conv.id)])?;
                continue;
            }
            exec(
                ctx.db,
                "UPDATE agent_jobs SET in_flight_at = ?1, attempts = attempts + 1, updated_at = ?1 WHERE conversation_id = ?2",
                &[json!(ctx.now_ms), json!(conv.id)],
            )?;
            jobs.push(ReplyJob {
                conversation_id: conv.id.clone(),
                trigger_message_id: item.trigger_message_id,
                attempts: item.attempts + 1,
                provider: cfg.provider,
                model: cfg.model.clone(),
                system_prompt: cfg.system_prompt.clone(),
                max_output_tokens: cfg.max_output_tokens,
                temperature: cfg.temperature,
                turns: history(ctx, &conv.id, cfg.history_limit)?,
            });
        }
        Ok(jobs)
    })
}

/// A rodada terminou. Se o contato escreveu DE NOVO enquanto o modelo pensava, a
/// conversa continua devendo resposta: reagenda com a mensagem nova como gatilho.
///
/// Sem isto some uma mensagem: o upsert de [`after_inbound`] não mexe em rodada
/// em andamento (senão o gatilho trocaria no meio), então quem escreveu durante a
/// chamada ao modelo ficaria sem resposta nenhuma — e calado, que é pior.
fn reagendar_ou_limpar<D: Db>(ctx: &Ctx<D>, job: &ReplyJob, debounce_ms: i64) -> Result<()> {
    // "chegou depois" é hora de CHEGADA (created_at), não hora da Meta: webhook
    // atrasado traz mensagem com timestamp antigo.
    let nova: Option<Value> = one(
        ctx.db,
        "SELECT m.id AS id FROM messages m, messages gatilho
         WHERE gatilho.id = ?2 AND m.conversation_id = ?1 AND m.direction = 'inbound'
           AND (m.created_at, m.id) > (gatilho.created_at, gatilho.id)
         ORDER BY m.created_at DESC, m.id DESC LIMIT 1",
        &[json!(job.conversation_id), json!(job.trigger_message_id)],
    )?;
    match nova.as_ref().and_then(|r| r.get("id")).and_then(Value::as_str) {
        Some(id) => exec(
            ctx.db,
            "UPDATE agent_jobs SET trigger_message_id = ?1, run_after = ?2, in_flight_at = NULL, attempts = 0,
                    last_error = NULL, updated_at = ?3
             WHERE conversation_id = ?4",
            &[json!(id), json!(ctx.now_ms + debounce_ms), json!(ctx.now_ms), json!(job.conversation_id)],
        ),
        None => exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(job.conversation_id)]),
    }
}

/// Tira o marcador de handoff do texto. Devolve o que sobra e se o modelo pediu
/// humano.
fn separar_handoff(texto: &str) -> (String, bool) {
    let pediu = texto.contains(HANDOFF_MARKER);
    (texto.replace(HANDOFF_MARKER, " ").trim().to_string(), pediu)
}

/// Grava o resultado de uma rodada marcada por [`claim_due`]: a resposta entra
/// pela cadeia de envio normal, o log recebe a linha, e o job sai da fila.
pub fn complete<D: Db>(ctx: &Ctx<D>, job: &ReplyJob, generated: &Generated) -> Result<AgentOutcomeKind> {
    let uso = |tokens_in, tokens_out, latency_ms| Uso {
        provider: job.provider,
        model: job.model.clone(),
        tokens_in,
        tokens_out,
        latency_ms,
    };
    ctx.db.atomic(|| {
        // Job já resolvido por outro caminho (handoff durante a chamada).
        let ainda: Option<Value> = one(
            ctx.db,
            "SELECT conversation_id FROM agent_jobs WHERE conversation_id = ?",
            &[json!(job.conversation_id)],
        )?;
        if ainda.is_none() {
            return Ok(AgentOutcomeKind::Skipped);
        }
        let limpar = || exec(ctx.db, "DELETE FROM agent_jobs WHERE conversation_id = ?", &[json!(job.conversation_id)]);

        match generated {
            Generated::Failed { code, message, retryable } => {
                if *retryable && job.attempts < MAX_ATTEMPTS {
                    exec(
                        ctx.db,
                        "UPDATE agent_jobs SET in_flight_at = NULL, run_after = ?1, last_error = ?2, updated_at = ?3
                         WHERE conversation_id = ?4",
                        &[
                            json!(ctx.now_ms + crate::outbox::backoff_ms(job.attempts)),
                            json!(format!("{code}: {message}")),
                            json!(ctx.now_ms),
                            json!(job.conversation_id),
                        ],
                    )?;
                    return Ok(AgentOutcomeKind::Failed);
                }
                // Desistiu: quem está do outro lado não pode ficar esperando.
                let u = uso(None, None, None);
                log_run(
                    ctx,
                    &job.conversation_id,
                    Some(&job.trigger_message_id),
                    None,
                    AgentOutcomeKind::Failed,
                    Some(&format!("{code}: {message}")),
                    Some(&u),
                )?;
                handoff(ctx, &job.conversation_id, "agent_failed")?;
                Ok(AgentOutcomeKind::Failed)
            }
            Generated::Reply { text, tokens_in, tokens_out, latency_ms } => {
                let u = uso(*tokens_in, *tokens_out, *latency_ms);
                let (corpo, pediu_humano) = separar_handoff(text);
                if corpo.is_empty() {
                    let reason = if pediu_humano { "modelo_pediu_humano" } else { "empty_reply" };
                    log_run(ctx, &job.conversation_id, Some(&job.trigger_message_id), None, AgentOutcomeKind::Handoff, Some(reason), Some(&u))?;
                    handoff(ctx, &job.conversation_id, reason)?;
                    return Ok(AgentOutcomeKind::Handoff);
                }
                let key = format!("{REPLY_IDEMPOTENCY_PREFIX}{}", job.trigger_message_id);
                let enviada = messaging::send_as(ctx, &job.conversation_id, Outgoing::Text { body: corpo }, Some(&key), SentVia::Ai);
                let sent = match enviada {
                    Ok(s) => s,
                    // Janela fechou, contato bloqueou, texto longo demais: é
                    // decisão do domínio, não falha do modelo. Fica no log.
                    Err(e @ (CoreError::Validation { .. } | CoreError::Conflict { .. })) => {
                        log_run(
                            ctx,
                            &job.conversation_id,
                            Some(&job.trigger_message_id),
                            None,
                            AgentOutcomeKind::Skipped,
                            Some(e.code()),
                            Some(&u),
                        )?;
                        limpar()?;
                        return Ok(AgentOutcomeKind::Skipped);
                    }
                    Err(e) => return Err(e),
                };
                log_run(
                    ctx,
                    &job.conversation_id,
                    Some(&job.trigger_message_id),
                    Some(&sent.message.id),
                    AgentOutcomeKind::Replied,
                    (!sent.created).then_some("replay"),
                    Some(&u),
                )?;
                if pediu_humano {
                    // Respondeu e chamou gente: o handoff apaga o job.
                    handoff(ctx, &job.conversation_id, "modelo_pediu_humano")?;
                    return Ok(AgentOutcomeKind::Replied);
                }
                exec(
                    ctx.db,
                    "UPDATE conversations SET status = ?1, status_changed_at = ?2, updated_at = ?2
                     WHERE id = ?3 AND status = 'open'",
                    &[json!(ConversationStatus::AiHandling.as_str()), json!(ctx.now_ms), json!(job.conversation_id)],
                )?;
                reagendar_ou_limpar(ctx, job, config(ctx)?.debounce_ms)?;
                Ok(AgentOutcomeKind::Replied)
            }
        }
    })
}

/// Quando o Alarm precisa voltar por causa do agente: próxima rodada devida, ou
/// o vencimento do prazo de uma rodada em andamento. `None` = nada pendente.
pub fn next_wake_at<D: Db>(ctx: &Ctx<D>) -> Result<Option<i64>> {
    let row: Option<Value> = one(
        ctx.db,
        "SELECT MIN(CASE WHEN in_flight_at IS NULL THEN run_after ELSE in_flight_at + ? END) AS at FROM agent_jobs",
        &[json!(IN_FLIGHT_LEASE_MS)],
    )?;
    Ok(row.and_then(|r| r.get("at").and_then(Value::as_i64)))
}
