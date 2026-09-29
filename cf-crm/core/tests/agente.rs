//! Fase 4: o agente responde o WhatsApp sozinho. SQLite de verdade, payload do
//! Kapso no formato v2, e o modelo dublado — o que se prova aqui é a DECISÃO
//! (quando responder, quando calar, quando chamar gente) e a garantia de que a
//! resposta automática não sai duas vezes.
mod common;

use common::{Env, NOW};
use crm_core::agent::{self, ConfigPatch, Generated, Scheduled};
use crm_core::inbox::{self, Ingested};
use crm_core::messaging::{self, Outgoing};
use crm_schema::kapso::{EventPayload, EVENT_MESSAGE_RECEIVED};
use crm_schema::model::{AgentOutcomeKind, ConversationStatus, MessageStatus, SentVia};
use serde_json::{json, Value};

const PNID: &str = "123456789012345";
const PHONE: &str = "+5511987654321";
const HORA: i64 = 60 * 60 * 1000;

/// `ts_ms` é a hora da Meta: ela vem em SEGUNDOS no payload, e é por ela que a
/// conversa se ordena — então mensagem de teste que não avança o relógio
/// empilha tudo no mesmo instante e esconde erro de ordem.
fn recebida(wamid: &str, texto: &str, tipo: &str, ts_ms: i64) -> EventPayload {
    let mut msg = json!({
        "id": wamid, "timestamp": (ts_ms / 1000).to_string(), "type": tipo,
        "kapso": { "direction": "inbound", "status": "received", "origin": "cloud_api", "has_media": false }
    });
    if tipo == "text" {
        msg["text"] = json!({ "body": texto });
        msg["kapso"]["content"] = json!(texto);
    }
    serde_json::from_value(json!({
        "message": msg,
        "conversation": { "id": "conv_1", "phone_number": PHONE, "phone_number_id": PNID, "kapso": { "contact_name": "Ana" } },
        "phone_number_id": PNID
    }))
    .unwrap()
}

/// Entrega uma mensagem do contato como o webhook do Kapso entrega.
fn entra(env: &Env, now: i64, wamid: &str, texto: &str) -> String {
    let r = inbox::ingest(&env.system(now), EVENT_MESSAGE_RECEIVED, wamid, &recebida(wamid, texto, "text", now)).unwrap();
    let Ingested::Stored { message_id } = r else { panic!("não gravou: {r:?}") };
    message_id
}

fn conversa_de(env: &Env, message_id: &str) -> String {
    messaging::get_message(&env.system(NOW), message_id).unwrap().conversation_id
}

fn conta(env: &Env, sql: &str) -> i64 {
    env.db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn config_ligada(prompt_extra: ConfigPatch) -> ConfigPatch {
    ConfigPatch {
        is_enabled: Some(true),
        system_prompt: Some("Você atende a clínica Bem Viver. Seja breve.".into()),
        debounce_ms: Some(5_000),
        ..prompt_extra
    }
}

fn ligar(env: &Env, patch: ConfigPatch) {
    agent::save_config(&env.ctx(NOW), config_ligada(patch)).unwrap();
}

fn resposta(texto: &str) -> Generated {
    Generated::Reply { text: texto.into(), tokens_in: Some(120), tokens_out: Some(30), latency_ms: Some(900) }
}

fn runs(env: &Env) -> Vec<(String, Option<String>)> {
    let c = env.system(NOW);
    agent::list_runs(&c, 50, None)
        .unwrap()
        .items
        .into_iter()
        .map(|r| (r.outcome.as_str().to_string(), r.reason))
        .collect()
}

fn ultima_saida(env: &Env, conv: &str) -> Option<crm_schema::model::Message> {
    messaging::list_messages(&env.system(NOW), conv, 50, None)
        .unwrap()
        .items
        .into_iter()
        .find(|m| m.sent_via == SentVia::Ai)
}

// ------------------------------------------------------------------ o caminho normal

#[test]
fn mensagem_do_contato_agenda_resposta_e_ela_sai_pela_outbox() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let msg = entra(&env, NOW, "wamid.in1", "oi, vocês atendem no sábado?");
    let conv = conversa_de(&env, &msg);

    // agendada para depois da espera, não agora
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);
    assert_eq!(conta(&env, "SELECT run_after FROM agent_jobs"), NOW + 5_000);
    assert!(agent::claim_due(&env.system(NOW), 5).unwrap().is_empty(), "respondeu antes da espera");

    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job.trigger_message_id, msg);
    assert_eq!(job.model, "claude-sonnet-5");
    assert_eq!(job.system_prompt, "Você atende a clínica Bem Viver. Seja breve.");
    assert_eq!(job.turns.len(), 1);
    assert_eq!(job.turns[0].role, "user");

    let out = agent::complete(&env.system(NOW + 6_000), job, &resposta("Atendemos sim, das 8h às 12h.")).unwrap();
    assert_eq!(out, AgentOutcomeKind::Replied);

    // a resposta é uma mensagem como qualquer outra: fila + outbox
    let m = ultima_saida(&env, &conv).expect("nenhuma resposta do agente");
    assert_eq!(m.status, MessageStatus::Queued);
    assert_eq!(m.body.as_deref(), Some("Atendemos sim, das 8h às 12h."));
    assert_eq!(m.idempotency_key, Some(format!("agent:{msg}")));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM outbox WHERE kind = 'send_message'"), 1);
    // e o envio de verdade é o mesmo do humano: o Alarm drena a outbox
    let envios = crm_core::outbox::claim_due(&env.system(NOW + 6_000), 10).unwrap();
    assert_eq!(envios.len(), 1);
    assert_eq!(envios[0].body["text"]["body"], "Atendemos sim, das 8h às 12h.");

    let conversa = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(conversa.status, ConversationStatus::AiHandling);
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    assert_eq!(runs(&env), vec![("replied".to_string(), None)]);
    let run = &agent::list_runs(&env.system(NOW), 5, None).unwrap().items[0];
    assert_eq!(run.tokens_in, Some(120));
    assert_eq!(run.model.as_deref(), Some("claude-sonnet-5"));
}

#[test]
fn tres_mensagens_seguidas_recebem_uma_resposta() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m1 = entra(&env, NOW, "wamid.a", "oi");
    entra(&env, NOW + 1_000, "wamid.b", "boa tarde");
    let m3 = entra(&env, NOW + 2_000, "wamid.c", "queria saber o preço da limpeza");
    let conv = conversa_de(&env, &m1);

    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1, "uma linha por conversa");
    // a espera reinicia a cada mensagem: quem ainda está escrevendo não é interrompido
    assert_eq!(conta(&env, "SELECT run_after FROM agent_jobs"), NOW + 2_000 + 5_000);

    let jobs = agent::claim_due(&env.system(NOW + 7_000), 5).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].trigger_message_id, m3, "a última mensagem é o gatilho");
    assert_eq!(jobs[0].turns.len(), 3, "o modelo vê as três");
    assert_eq!(jobs[0].turns[2].text, "queria saber o preço da limpeza");

    agent::complete(&env.system(NOW + 7_000), &jobs[0], &resposta("A limpeza custa R$ 180.")).unwrap();
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM messages WHERE direction = 'outbound'"), 1);
    assert!(ultima_saida(&env, &conv).is_some());
}

/// O pior caso do agente: o DO cai depois de gravar a resposta e antes de apagar
/// o job. A rodada volta, e a chave de idempotência segura o reenvio.
#[test]
fn rodada_repetida_nao_responde_duas_vezes() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let msg = entra(&env, NOW, "wamid.in1", "oi");
    let conv = conversa_de(&env, &msg);
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    agent::complete(&env.system(NOW + 5_000), &jobs[0], &resposta("Olá! Como posso ajudar?")).unwrap();

    // a queda: o job voltou para a fila com o mesmo gatilho
    env.db
        .conn
        .execute(
            "INSERT INTO agent_jobs (conversation_id, trigger_message_id, run_after, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3, ?3)",
            rusqlite::params![conv, msg, NOW + 10_000],
        )
        .unwrap();
    let jobs = agent::claim_due(&env.system(NOW + 10_000), 5).unwrap();
    assert_eq!(jobs.len(), 1);
    let out = agent::complete(&env.system(NOW + 10_000), &jobs[0], &resposta("Olá de novo!")).unwrap();
    assert_eq!(out, AgentOutcomeKind::Replied);

    assert_eq!(conta(&env, "SELECT COUNT(*) FROM messages WHERE direction = 'outbound'"), 1, "respondeu duas vezes");
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM outbox"), 1);
    assert_eq!(runs(&env)[0], ("replied".to_string(), Some("replay".to_string())));
}

#[test]
fn historico_vai_na_ordem_da_conversa_com_o_papel_certo() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m1 = entra(&env, NOW, "wamid.a", "bom dia");
    let conv = conversa_de(&env, &m1);
    messaging::send(&env.ctx(NOW + 1_000), &conv, Outgoing::Text { body: "Bom dia! Em que posso ajudar?".into() }, None).unwrap();
    // a resposta do atendente calou o agente; devolve a conversa a ele
    agent::back_to_ai(&env.ctx(NOW + 2_000), &conv).unwrap();
    entra(&env, NOW + 3_000, "wamid.b", "quero marcar avaliação");

    let jobs = agent::claim_due(&env.system(NOW + 9_000), 5).unwrap();
    let turns: Vec<(&str, &str)> = jobs[0].turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        turns,
        vec![
            ("user", "bom dia"),
            ("assistant", "Bom dia! Em que posso ajudar?"),
            ("user", "quero marcar avaliação"),
        ]
    );
}

// ------------------------------------------------------------------ quando o agente NÃO fala

#[test]
fn agente_desligado_nao_agenda_e_nao_deixa_log() {
    let env = Env::new();
    let msg = entra(&env, NOW, "wamid.in1", "oi");
    let conv = conversa_de(&env, &msg);
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    // nada de uma linha de log por mensagem em instalação que não usa agente
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_runs"), 0);
    assert_eq!(messaging::get_conversation(&env.system(NOW), &conv).unwrap().status, ConversationStatus::Open);
}

#[test]
fn atendente_respondendo_cala_o_agente_e_a_proxima_mensagem_fica_com_ele() {
    let env = Env::new();
    ligar(&env, ConfigPatch { human_silence_ms: Some(2 * HORA), ..ConfigPatch::default() });
    let m1 = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m1);

    messaging::send(&env.ctx(NOW + 1_000), &conv, Outgoing::Text { body: "Oi, Ana! Eu te ajudo.".into() }, None).unwrap();
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human);
    assert_eq!(c.bot_silenced_until, Some(NOW + 1_000 + 2 * HORA));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0, "a rodada pendente tinha de ser cancelada");

    let m2 = entra(&env, NOW + 2_000, "wamid.b", "obrigada!");
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    assert_eq!(runs(&env)[0], ("skipped".to_string(), Some("human_handling".to_string())));
    let _ = m2;

    // passada a silenciada, quem devolve a conversa ao agente é uma pessoa
    agent::back_to_ai(&env.ctx(NOW + 3 * HORA), &conv).unwrap();
    entra(&env, NOW + 3 * HORA + 1_000, "wamid.c", "voltei");
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);
}

#[test]
fn equipe_respondendo_pelo_app_do_whatsapp_tambem_cala_o_agente() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m1 = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m1);
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);

    // mensagem que a equipe mandou pelo celular, fora do CRM: o Kapso avisa
    // com origin 'business_app', e ela entra como saída de outro dispositivo
    let eco: EventPayload = serde_json::from_value(json!({
        "message": {
            "id": "wamid.app", "timestamp": ((NOW + 1_000) / 1000).to_string(), "type": "text",
            "text": { "body": "Oi, Ana! Já te respondo." },
            "kapso": { "direction": "outbound", "status": "sent", "origin": "business_app", "has_media": false }
        },
        "conversation": { "id": "conv_1", "phone_number": PHONE, "phone_number_id": PNID },
        "phone_number_id": PNID
    }))
    .unwrap();
    inbox::ingest(&env.system(NOW + 1_000), "whatsapp.message.sent", "k-app", &eco).unwrap();

    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human);
    assert!(c.bot_silenced_until.is_some_and(|t| t > NOW + 1_000));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0, "o robô ia falar em cima do atendente");
}

#[test]
fn pedido_de_saida_bloqueia_o_contato_e_cancela_a_resposta_na_fila() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m1 = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m1);
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);

    entra(&env, NOW + 1_000, "wamid.b", "pare de me mandar mensagem");
    let contato = crm_core::contacts::get(&env.system(NOW), &messaging::get_conversation(&env.system(NOW), &conv).unwrap().contact_id).unwrap();
    assert!(contato.is_blocked);
    assert_eq!(contato.blocked_reason.as_deref(), Some("opt_out"));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0, "a resposta que estava na fila não pode sair");
    assert_eq!(runs(&env)[0], ("skipped".to_string(), Some("opt_out".to_string())));
}

#[test]
fn duvida_com_a_palavra_parar_no_meio_nao_bloqueia_e_recebe_resposta() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "tem como parar a dor até a consulta?");
    let conv = conversa_de(&env, &m);
    let contato = crm_core::contacts::get(&env.system(NOW), &messaging::get_conversation(&env.system(NOW), &conv).unwrap().contact_id).unwrap();
    assert!(!contato.is_blocked, "paciente bloqueado por perguntar da dor");
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);
}

#[test]
fn pedido_de_atendente_vai_para_humano_sem_gastar_modelo() {
    let env = Env::new();
    ligar(
        &env,
        ConfigPatch { handoff_keywords: Some(vec!["falar com atendente".into(), "quero um humano".into()]), ..ConfigPatch::default() },
    );
    let m = entra(&env, NOW, "wamid.a", "Quero FALAR COM ATENDENTE, por favor");
    let conv = conversa_de(&env, &m);

    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human);
    assert_eq!(c.last_handoff_reason.as_deref(), Some("handoff_keyword"));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    assert_eq!(runs(&env)[0], ("handoff".to_string(), Some("handoff_keyword".to_string())));
}

#[test]
fn sinal_ambiguo_de_saida_cala_o_agente_sem_bloquear_ninguem() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "me deixa em paz");
    let conv = conversa_de(&env, &m);
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human, "quem confirma o descadastro é uma pessoa");
    let contato = crm_core::contacts::get(&env.system(NOW), &c.contact_id).unwrap();
    assert!(!contato.is_blocked);
    assert_eq!(runs(&env)[0], ("handoff".to_string(), Some("opt_out_provavel".to_string())));
}

#[test]
fn foto_sem_legenda_fica_para_gente_ver() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let r = inbox::ingest(&env.system(NOW), EVENT_MESSAGE_RECEIVED, "wamid.img", &recebida("wamid.img", "", "image", NOW)).unwrap();
    assert!(matches!(r, Ingested::Stored { .. }));
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    assert_eq!(runs(&env)[0], ("skipped".to_string(), Some("unsupported_inbound_type".to_string())));
    // e continua não lida: o contador é o que chama gente
    assert_eq!(conta(&env, "SELECT unread_count FROM conversations"), 1);
}

#[test]
fn janela_de_24h_fechada_nao_responde() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m);
    // o Alarm só voltou no dia seguinte
    let jobs = agent::claim_due(&env.system(NOW + 25 * HORA), 5).unwrap();
    assert!(jobs.is_empty());
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    assert_eq!(runs(&env)[0], ("skipped".to_string(), Some("service_window_closed".to_string())));
    assert!(ultima_saida(&env, &conv).is_none());
}

#[test]
fn teto_de_respostas_por_hora_entrega_a_conversa_a_um_humano() {
    let env = Env::new();
    ligar(&env, ConfigPatch { max_replies_per_hour: Some(1), ..ConfigPatch::default() });
    let m1 = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m1);
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    agent::complete(&env.system(NOW + 5_000), &jobs[0], &resposta("Olá!")).unwrap();

    entra(&env, NOW + 6_000, "wamid.b", "oi de novo");
    let jobs = agent::claim_due(&env.system(NOW + 12_000), 5).unwrap();
    assert!(jobs.is_empty(), "passou do teto e respondeu");
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human, "laço de robô com robô tem de chamar gente");
    assert_eq!(c.last_handoff_reason.as_deref(), Some("rate_limited"));
}

// ------------------------------------------------------------------ o modelo falhando

#[test]
fn erro_temporario_tenta_de_novo_e_no_fim_chama_humano() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "oi");
    let conv = conversa_de(&env, &m);
    let falha = || Generated::Failed { code: "overloaded_error".into(), message: "sobrecarregado".into(), retryable: true };

    let mut agora = NOW + 5_000;
    for tentativa in 1..=agent::MAX_ATTEMPTS {
        let jobs = agent::claim_due(&env.system(agora), 5).unwrap();
        assert_eq!(jobs.len(), 1, "tentativa {tentativa} não foi entregue");
        assert_eq!(jobs[0].attempts, tentativa);
        agent::complete(&env.system(agora), &jobs[0], &falha()).unwrap();
        agora += HORA;
    }
    // esgotou: ninguém fica esperando um robô que desistiu
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 0);
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human);
    assert_eq!(c.last_handoff_reason.as_deref(), Some("agent_failed"));
    assert_eq!(runs(&env)[0].0, "failed");
    assert!(ultima_saida(&env, &conv).is_none());
}

#[test]
fn rodada_interrompida_volta_para_a_fila_depois_do_prazo() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    entra(&env, NOW, "wamid.a", "oi");
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    assert_eq!(jobs.len(), 1);
    // o DO caiu no meio da chamada: ninguém chamou complete
    assert!(agent::claim_due(&env.system(NOW + 6_000), 5).unwrap().is_empty(), "entregou a mesma rodada duas vezes");
    let de_novo = agent::claim_due(&env.system(NOW + 5_000 + agent::IN_FLIGHT_LEASE_MS + 1), 5).unwrap();
    assert_eq!(de_novo.len(), 1);
    assert_eq!(de_novo[0].attempts, 2);
}

#[test]
fn modelo_pedindo_humano_responde_e_entrega_a_conversa() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "quanto custa o implante?");
    let conv = conversa_de(&env, &m);
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    let out = agent::complete(
        &env.system(NOW + 5_000),
        &jobs[0],
        &resposta("Isso quem responde é a nossa equipe, já vou chamar. [HUMANO]"),
    )
    .unwrap();

    assert_eq!(out, AgentOutcomeKind::Replied);
    let saida = ultima_saida(&env, &conv).unwrap();
    assert_eq!(saida.body.as_deref(), Some("Isso quem responde é a nossa equipe, já vou chamar."), "o marcador não vai pro cliente");
    let c = messaging::get_conversation(&env.system(NOW), &conv).unwrap();
    assert_eq!(c.status, ConversationStatus::Human);
    assert_eq!(c.last_handoff_reason.as_deref(), Some("modelo_pediu_humano"));
}

#[test]
fn resposta_vazia_do_modelo_vira_handoff_em_vez_de_mensagem_em_branco() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m = entra(&env, NOW, "wamid.a", "???");
    let conv = conversa_de(&env, &m);
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    let out = agent::complete(&env.system(NOW + 5_000), &jobs[0], &resposta("   ")).unwrap();
    assert_eq!(out, AgentOutcomeKind::Handoff);
    assert!(ultima_saida(&env, &conv).is_none());
    assert_eq!(messaging::get_conversation(&env.system(NOW), &conv).unwrap().status, ConversationStatus::Human);
}

// ------------------------------------------------------------------ config e Alarm

#[test]
fn ligar_o_agente_sem_prompt_e_recusado() {
    let env = Env::new();
    let e = agent::save_config(&env.ctx(NOW), ConfigPatch { is_enabled: Some(true), ..ConfigPatch::default() }).unwrap_err();
    assert_eq!(e.code(), "agent_prompt_required");
    assert!(!agent::config(&env.system(NOW)).unwrap().is_enabled);
}

#[test]
fn config_nova_nasce_desligada_e_com_padroes() {
    let env = Env::new();
    let c = agent::config(&env.system(NOW)).unwrap();
    assert!(!c.is_enabled, "instalação nova não começa respondendo em nome de ninguém");
    assert_eq!(c.debounce_ms, 8_000);
    assert_eq!(c.max_replies_per_hour, 12);
    assert_eq!(c.history_limit, 20);
}

#[test]
fn valor_fora_da_faixa_e_recusado() {
    let env = Env::new();
    let e = agent::save_config(&env.ctx(NOW), ConfigPatch { debounce_ms: Some(999_999), ..ConfigPatch::default() }).unwrap_err();
    assert_eq!(e.code(), "invalid_agent_config");
}

#[test]
fn o_alarm_acorda_para_a_resposta_do_agente() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    assert_eq!(agent::next_wake_at(&env.system(NOW)).unwrap(), None);
    entra(&env, NOW, "wamid.a", "oi");
    assert_eq!(agent::next_wake_at(&env.system(NOW)).unwrap(), Some(NOW + 5_000));
}

/// A mensagem que chega ENQUANTO o modelo pensa não pode sumir: o upsert da
/// ingestão não mexe em rodada em andamento, então quem reagenda é o `complete`.
#[test]
fn mensagem_que_chega_durante_a_rodada_ganha_a_resposta_seguinte() {
    let env = Env::new();
    ligar(&env, ConfigPatch::default());
    let m1 = entra(&env, NOW, "wamid.a", "oi, quanto custa a limpeza?");
    let jobs = agent::claim_due(&env.system(NOW + 5_000), 5).unwrap();
    assert_eq!(jobs[0].trigger_message_id, m1);

    // o contato escreve de novo com a rodada em andamento
    let m2 = entra(&env, NOW + 6_000, "wamid.b", "e vocês atendem no sábado?");
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs WHERE in_flight_at IS NOT NULL"), 1);

    agent::complete(&env.system(NOW + 7_000), &jobs[0], &resposta("A limpeza custa R$ 180.")).unwrap();

    // a rodada não foi apagada: virou a próxima, com a mensagem nova de gatilho
    assert_eq!(conta(&env, "SELECT COUNT(*) FROM agent_jobs"), 1);
    let jobs = agent::claim_due(&env.system(NOW + 7_000 + 5_000), 5).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].trigger_message_id, m2, "a pergunta do sábado ficaria sem resposta");
    assert_eq!(jobs[0].attempts, 1, "rodada nova começa do zero");
}
