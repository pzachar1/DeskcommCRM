-- Fase 4: o agente responde o WhatsApp sozinho.
--
-- Três tabelas, uma por pergunta:
--   agent_config  o que o agente é (e se está ligado) NESTE tenant
--   agent_jobs    que conversa está devendo resposta, e a partir de quando
--   agent_runs    o que aconteceu em cada rodada — é o log que a tela mostra
--
-- O envio em si NÃO muda: a resposta entra por `messages` + `outbox`, na mesma
-- transação, e quem fala com o Kapso continua sendo o Alarm do DO. O agente
-- decide O QUE dizer; a cadeia de envio, com a garantia de sair no máximo uma
-- vez, é a mesma da mensagem que um humano manda pela tela.

-- Uma linha só (id = 'default'): a config é DO TENANT, e o tenant é este banco.
-- Linha ausente = agente desligado (o padrão vem do Rust, AgentConfig::default).
CREATE TABLE agent_config (
  id                TEXT PRIMARY KEY CHECK (id = 'default'),
  is_enabled        INTEGER NOT NULL DEFAULT 0 CHECK (is_enabled IN (0, 1)),
  -- 'anthropic' fala com a Messages API (direto ou pelo AI Gateway);
  -- 'workers_ai' fala com o /ai/run da Cloudflare. Quem resolve URL e chave é o Worker.
  provider          TEXT NOT NULL DEFAULT 'anthropic' CHECK (provider IN ('anthropic', 'workers_ai')),
  model             TEXT NOT NULL DEFAULT 'claude-sonnet-5' CHECK (length(trim(model)) BETWEEN 1 AND 200),
  system_prompt     TEXT NOT NULL DEFAULT '' CHECK (length(system_prompt) <= 20000),
  max_output_tokens INTEGER NOT NULL DEFAULT 400 CHECK (max_output_tokens BETWEEN 16 AND 4096),
  temperature       REAL NOT NULL DEFAULT 0.3 CHECK (temperature BETWEEN 0 AND 2),
  -- quantas mensagens da conversa vão no prompt (as mais recentes)
  history_limit     INTEGER NOT NULL DEFAULT 20 CHECK (history_limit BETWEEN 1 AND 100),
  -- espera depois da última mensagem do contato: quem escreve em três linhas
  -- recebe UMA resposta, não três. Zero = responde na hora.
  debounce_ms       INTEGER NOT NULL DEFAULT 8000 CHECK (debounce_ms BETWEEN 0 AND 120000),
  -- depois que um humano responde, o agente fica quieto por este tempo na conversa
  human_silence_ms  INTEGER NOT NULL DEFAULT 14400000 CHECK (human_silence_ms BETWEEN 0 AND 604800000),
  -- teto de respostas automáticas por hora POR CONVERSA: se estourar, o agente
  -- para e a conversa vai para humano. Guarda contra laço com outro robô.
  max_replies_per_hour INTEGER NOT NULL DEFAULT 12 CHECK (max_replies_per_hour BETWEEN 1 AND 120),
  -- frases do CONTATO que passam a conversa para humano sem gastar modelo
  handoff_keywords  TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(handoff_keywords)),
  updated_by        TEXT,
  updated_at        INTEGER NOT NULL
);

-- Uma linha por conversa (PK), então mensagem nova do contato durante a espera
-- só empurra o horário: cinco mensagens seguidas = uma resposta.
CREATE TABLE agent_jobs (
  conversation_id    TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
  -- a mensagem do contato que pediu esta resposta. Vira a chave de idempotência
  -- do envio (`agent:<id>`), então retry depois de queda não responde duas vezes.
  trigger_message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  run_after          INTEGER NOT NULL,
  attempts           INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  -- rodada em andamento desde este instante. Diferente do envio: repetir a
  -- chamada ao modelo só custa token, e a idempotência do envio segura o resto.
  in_flight_at       INTEGER,
  last_error         TEXT,
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX idx_agent_jobs_due ON agent_jobs (run_after) WHERE in_flight_at IS NULL;

-- Toda rodada deixa linha, inclusive a que decidiu NÃO responder: sem isto,
-- "o agente ficou calado" não tem onde ser investigado.
CREATE TABLE agent_runs (
  id                 TEXT PRIMARY KEY,
  conversation_id    TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  trigger_message_id TEXT REFERENCES messages(id) ON DELETE SET NULL,
  reply_message_id   TEXT REFERENCES messages(id) ON DELETE SET NULL,
  outcome            TEXT NOT NULL CHECK (outcome IN ('replied', 'handoff', 'skipped', 'failed')),
  -- por que: 'agent_disabled', 'bot_silenced', 'service_window_closed',
  -- 'handoff_keyword', 'rate_limited', o código do provedor no erro...
  reason             TEXT,
  provider           TEXT,
  model              TEXT,
  tokens_in          INTEGER CHECK (tokens_in IS NULL OR tokens_in >= 0),
  tokens_out         INTEGER CHECK (tokens_out IS NULL OR tokens_out >= 0),
  latency_ms         INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
  created_at         INTEGER NOT NULL,
  CHECK ((outcome = 'replied') = (reply_message_id IS NOT NULL))
);
CREATE INDEX idx_agent_runs_recent ON agent_runs (created_at DESC);
CREATE INDEX idx_agent_runs_conversation ON agent_runs (conversation_id, created_at DESC);
