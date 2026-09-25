-- SQLite do Durable Object de UM tenant.
-- Não existe organization_id: o isolamento é o próprio DO (um por tenant,
-- idFromName(tenant_id)). Uma query aqui não tem como enxergar outro tenant.
--
-- Convenções (iguais ao D1 global):
--   ids      TEXT, UUIDv7
--   tempo    INTEGER, ms desde epoch, UTC
--   booleano INTEGER 0/1 com CHECK
--   json     TEXT com CHECK (json_valid(...))
--   ordem    TEXT com chave de fractional indexing (ver README: por que não REAL)
--   usuário  user_id TEXT aponta para users.id do D1, sem FK (está em outro banco)

-- Guarda contra erro de roteamento: o DO grava o próprio tenant na 1ª carga
-- e recusa request cujo tenant não bate.
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- ---------------------------------------------------------------- contatos

CREATE TABLE contacts (
  id               TEXT PRIMARY KEY,
  name             TEXT,
  display_name     TEXT,              -- nome de perfil do WhatsApp
  email            TEXT CHECK (email IS NULL OR email LIKE '_%@_%._%'),
  email_normalized TEXT GENERATED ALWAYS AS (lower(trim(email))) STORED,
  phone_e164       TEXT CHECK (phone_e164 IS NULL OR (phone_e164 GLOB '+[1-9]*'
                                  AND phone_e164 NOT GLOB '+*[^0-9]*'
                                  AND length(phone_e164) BETWEEN 9 AND 16)),
  -- wa_id é o identificador que a Meta manda no webhook (só dígitos).
  -- Fica separado do telefone: no Brasil o wa_id de número antigo pode vir
  -- SEM o nono dígito, e casar por phone_e164 duplicaria o contato.
  wa_id            TEXT CHECK (wa_id IS NULL OR (wa_id NOT GLOB '*[^0-9]*' AND length(wa_id) BETWEEN 8 AND 15)),
  is_blocked       INTEGER NOT NULL DEFAULT 0 CHECK (is_blocked IN (0, 1)),
  blocked_reason   TEXT,              -- 'opt_out' | 'manual' | ...
  blocked_at       INTEGER,
  is_anonymized    INTEGER NOT NULL DEFAULT 0 CHECK (is_anonymized IN (0, 1)),
  anonymized_at    INTEGER,
  merged_into_id   TEXT REFERENCES contacts(id) ON DELETE RESTRICT,
  merged_at        INTEGER,
  force_human      INTEGER NOT NULL DEFAULT 0 CHECK (force_human IN (0, 1)),
  consent          TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(consent)),
  source           TEXT NOT NULL DEFAULT 'manual',
  source_metadata  TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(source_metadata)),
  last_activity_at INTEGER,
  created_by       TEXT,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  CHECK (is_blocked = 0 OR blocked_at IS NOT NULL),
  CHECK (is_anonymized = 0 OR anonymized_at IS NOT NULL),
  CHECK ((merged_into_id IS NULL) = (merged_at IS NULL)),
  CHECK (merged_into_id IS NULL OR merged_into_id <> id)
);
-- Identidade única só entre contatos vivos (mesclado não conta)
CREATE UNIQUE INDEX uq_contacts_wa_id  ON contacts (wa_id)            WHERE wa_id IS NOT NULL AND merged_into_id IS NULL;
CREATE UNIQUE INDEX uq_contacts_phone  ON contacts (phone_e164)       WHERE phone_e164 IS NOT NULL AND merged_into_id IS NULL;
CREATE UNIQUE INDEX uq_contacts_email  ON contacts (email_normalized) WHERE email_normalized IS NOT NULL AND merged_into_id IS NULL;
CREATE INDEX idx_contacts_last_activity ON contacts (last_activity_at DESC);

-- ---------------------------------------------------------------- tags
-- SQLite não tem text[]; tag vira tabela e junção indexada.

CREATE TABLE tags (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
  color      TEXT CHECK (color IS NULL OR (length(color) = 7 AND color GLOB '#[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]')),
  created_at INTEGER NOT NULL
);

CREATE TABLE contact_tags (
  contact_id TEXT NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
  tag_id     TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (contact_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX idx_contact_tags_tag ON contact_tags (tag_id);

-- ---------------------------------------------------------------- funil

CREATE TABLE pipelines (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  slug        TEXT NOT NULL UNIQUE CHECK (slug NOT GLOB '*[^a-z0-9_-]*' AND length(slug) BETWEEN 2 AND 40),
  description TEXT,
  is_default  INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
  is_archived INTEGER NOT NULL DEFAULT 0 CHECK (is_archived IN (0, 1)),
  position    TEXT NOT NULL,
  -- renomeia lead/deal/won/lost por nicho (lead=Cliente, won=Pago...)
  vocabulary  TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(vocabulary)),
  -- { fields: [...], lost_reasons: [...] } — schema dos custom_fields do lead
  settings    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(settings)),
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE UNIQUE INDEX uq_pipelines_one_default ON pipelines (is_default) WHERE is_default = 1;

CREATE TABLE stages (
  id             TEXT PRIMARY KEY,
  pipeline_id    TEXT NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
  name           TEXT NOT NULL,
  slug           TEXT NOT NULL CHECK (slug NOT GLOB '*[^a-z0-9_-]*' AND length(slug) BETWEEN 2 AND 40),
  description    TEXT,
  position       TEXT NOT NULL,
  color          TEXT CHECK (color IS NULL OR (length(color) = 7 AND color GLOB '#[0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F][0-9a-fA-F]')),
  is_won         INTEGER NOT NULL DEFAULT 0 CHECK (is_won IN (0, 1)),
  is_lost        INTEGER NOT NULL DEFAULT 0 CHECK (is_lost IN (0, 1)),
  is_archived    INTEGER NOT NULL DEFAULT 0 CHECK (is_archived IN (0, 1)),
  requires_human INTEGER NOT NULL DEFAULT 0 CHECK (requires_human IN (0, 1)),
  expected_duration_hours INTEGER CHECK (expected_duration_hours IS NULL OR expected_duration_hours > 0),
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  CHECK (NOT (is_won = 1 AND is_lost = 1)),
  UNIQUE (pipeline_id, slug),
  UNIQUE (id, pipeline_id)      -- alvo da FK composta de leads
);
CREATE INDEX idx_stages_pipeline_position ON stages (pipeline_id, position);

CREATE TABLE leads (
  id                  TEXT PRIMARY KEY,
  pipeline_id         TEXT NOT NULL,
  stage_id            TEXT NOT NULL,
  -- contato não é apagado (LGPD anonimiza), então RESTRICT: nada de cascade fantasma
  contact_id          TEXT REFERENCES contacts(id) ON DELETE RESTRICT,
  title               TEXT NOT NULL,
  description         TEXT,
  status              TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'won', 'lost')),
  lost_reason         TEXT,
  position_in_stage   TEXT NOT NULL,
  value_cents         INTEGER CHECK (value_cents IS NULL OR value_cents >= 0),
  currency            TEXT NOT NULL DEFAULT 'BRL' CHECK (length(currency) = 3 AND currency NOT GLOB '*[^A-Z]*'),
  owner_user_id       TEXT,
  assigned_at         INTEGER,
  expected_close_date TEXT CHECK (expected_close_date IS NULL OR date(expected_close_date) = expected_close_date),
  closed_at           INTEGER,
  last_activity_at    INTEGER,
  source              TEXT NOT NULL DEFAULT 'manual',
  source_metadata     TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(source_metadata)),
  external_id         TEXT,
  custom_fields       TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(custom_fields)),
  created_by          TEXT,
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  -- a etapa TEM de ser do mesmo funil do lead; no Deskcomm isso era só convenção
  FOREIGN KEY (stage_id, pipeline_id) REFERENCES stages (id, pipeline_id) ON DELETE RESTRICT,
  CHECK ((status = 'open' AND closed_at IS NULL) OR (status IN ('won', 'lost') AND closed_at IS NOT NULL)),
  CHECK (status <> 'lost' OR length(trim(coalesce(lost_reason, ''))) > 0)
);
CREATE INDEX idx_leads_stage_position ON leads (stage_id, position_in_stage);
CREATE INDEX idx_leads_pipeline_status ON leads (pipeline_id, status);
CREATE INDEX idx_leads_contact ON leads (contact_id);
CREATE INDEX idx_leads_owner_open ON leads (owner_user_id) WHERE status = 'open';
CREATE INDEX idx_leads_last_activity ON leads (last_activity_at DESC);
CREATE UNIQUE INDEX uq_leads_source_external ON leads (source, external_id) WHERE external_id IS NOT NULL;

CREATE TABLE lead_tags (
  lead_id    TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
  tag_id     TEXT NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (lead_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX idx_lead_tags_tag ON lead_tags (tag_id);

-- Timeline polimórfica. `type` é vocabulário ABERTO: sem CHECK aqui,
-- o enum vive no Rust (ActivityType) e o emissor usa a constante.
CREATE TABLE lead_activities (
  id            TEXT PRIMARY KEY,
  lead_id       TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
  contact_id    TEXT REFERENCES contacts(id) ON DELETE RESTRICT,
  type          TEXT NOT NULL,
  source_module TEXT NOT NULL,      -- 'crm' | 'whatsapp' | 'ai' | 'automation' | 'api'
  source_id     TEXT,               -- id da linha que originou (ex.: messages.id)
  payload       TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload)),
  performed_by  TEXT,               -- user_id; NULL = sistema/IA
  performed_at  INTEGER NOT NULL,
  created_at    INTEGER NOT NULL
);
CREATE INDEX idx_lead_activities_lead ON lead_activities (lead_id, performed_at DESC);
CREATE INDEX idx_lead_activities_contact ON lead_activities (contact_id, performed_at DESC);
CREATE UNIQUE INDEX uq_lead_activities_source ON lead_activities (lead_id, type, source_module, source_id) WHERE source_id IS NOT NULL;

-- Vínculos polimórficos, com target_kind fechado (anti-pattern 8)
CREATE TABLE lead_links (
  id          TEXT PRIMARY KEY,
  lead_id     TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
  target_kind TEXT NOT NULL CHECK (target_kind IN ('contact', 'conversation', 'message', 'lead', 'order', 'appointment', 'external')),
  target_id   TEXT NOT NULL,
  link_kind   TEXT NOT NULL,
  metadata    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
  created_by  TEXT,
  created_at  INTEGER NOT NULL,
  UNIQUE (lead_id, target_kind, target_id, link_kind)
);
CREATE INDEX idx_lead_links_target ON lead_links (target_kind, target_id);

-- ---------------------------------------------------------------- WhatsApp (Kapso)

-- Uma conversa por contato POR número do tenant (D1 whatsapp_numbers).
-- A janela de 24h da Meta NÃO é coluna: é last_inbound_at + 24h, calculada.
CREATE TABLE conversations (
  id                  TEXT PRIMARY KEY,
  contact_id          TEXT NOT NULL REFERENCES contacts(id) ON DELETE RESTRICT,
  phone_number_id     TEXT NOT NULL,
  -- conversation.id do payload v2 do Kapso. Só ponteiro: o estado da conversa é nosso.
  kapso_conversation_id TEXT UNIQUE,
  status              TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'ai_handling', 'human', 'closed')),
  status_changed_at   INTEGER NOT NULL,
  assigned_to_user_id TEXT,
  assigned_at         INTEGER,
  last_inbound_at     INTEGER,
  last_outbound_at    INTEGER,
  last_message_at     INTEGER,
  last_message_preview TEXT,
  unread_count        INTEGER NOT NULL DEFAULT 0 CHECK (unread_count >= 0),
  bot_silenced_until  INTEGER,
  last_handoff_at     INTEGER,
  last_handoff_reason TEXT,
  snooze_until        INTEGER,
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  CHECK ((assigned_to_user_id IS NULL) = (assigned_at IS NULL)),
  UNIQUE (contact_id, phone_number_id)
);
CREATE INDEX idx_conversations_inbox ON conversations (status, last_message_at DESC);
CREATE INDEX idx_conversations_assignee ON conversations (assigned_to_user_id, status) WHERE assigned_to_user_id IS NOT NULL;
CREATE INDEX idx_conversations_snooze ON conversations (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE TABLE messages (
  id                TEXT PRIMARY KEY,
  conversation_id   TEXT NOT NULL REFERENCES conversations(id) ON DELETE RESTRICT,
  contact_id        TEXT NOT NULL REFERENCES contacts(id) ON DELETE RESTRICT,
  -- wamid da Meta. NULL enquanto a saída está na fila (ainda não aceita).
  external_id       TEXT,
  -- chave de idempotência do ENVIO: header Idempotency-Key da API, ou id
  -- gerado pelo agente/automação. Retry da Queue reusa a mesma chave.
  idempotency_key   TEXT,
  direction         TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
  type              TEXT NOT NULL CHECK (type IN ('text', 'image', 'video', 'audio', 'document', 'sticker',
                                                  'location', 'contacts', 'reaction', 'interactive', 'button',
                                                  'template', 'system', 'unsupported')),
  -- queued -> accepted (API do Kapso devolveu wamid) -> sent -> delivered -> read
  -- failed pode vir de qualquer estado antes de delivered. Inbound é sempre 'received'.
  status            TEXT NOT NULL CHECK (status IN ('queued', 'accepted', 'sent', 'delivered', 'read', 'failed', 'received')),
  body              TEXT,
  template_name     TEXT,
  template_language TEXT,
  template_params   TEXT CHECK (template_params IS NULL OR json_valid(template_params)),
  media_r2_key      TEXT,             -- mídia copiada para o R2 (URL da Meta expira)
  media_meta_id     TEXT,             -- id da mídia na Meta, para baixar sob demanda
  media_mime        TEXT,
  media_size_bytes  INTEGER CHECK (media_size_bytes IS NULL OR media_size_bytes >= 0),
  reply_to_external_id TEXT,
  sent_via          TEXT NOT NULL CHECK (sent_via IN ('contact', 'crm', 'ai', 'automation', 'api', 'external_device', 'system')),
  sent_by_user_id   TEXT,
  error_code        TEXT,
  error_message     TEXT,
  sent_at           INTEGER NOT NULL, -- inbound: timestamp da Meta; outbound: hora em que entrou na fila
  accepted_at       INTEGER,
  delivered_at      INTEGER,
  read_at           INTEGER,
  failed_at         INTEGER,
  metadata          TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  CHECK ((direction = 'inbound') = (status = 'received')),
  CHECK ((direction = 'inbound') = (sent_via = 'contact')),
  CHECK (direction = 'outbound' OR external_id IS NOT NULL),
  CHECK (status NOT IN ('accepted', 'sent', 'delivered', 'read') OR external_id IS NOT NULL),
  CHECK (type <> 'template' OR (template_name IS NOT NULL AND template_language IS NOT NULL)),
  CHECK (status <> 'failed' OR failed_at IS NOT NULL)
);
-- Idempotência: webhook repetido do Kapso bate aqui e vira no-op (ON CONFLICT DO NOTHING)
CREATE UNIQUE INDEX uq_messages_external ON messages (external_id) WHERE external_id IS NOT NULL;
CREATE UNIQUE INDEX uq_messages_idempotency ON messages (idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX idx_messages_conversation ON messages (conversation_id, sent_at DESC);
CREATE INDEX idx_messages_pending ON messages (status, created_at) WHERE status IN ('queued', 'failed');

-- Webhook de status chega fora de ordem (read antes de delivered é comum).
-- Status só avança; a atualização que regride é descartada em silêncio.
-- Trigger local de SQLite, sem rede: não fere a regra "trigger não faz HTTP".
CREATE TRIGGER trg_messages_status_monotonic
BEFORE UPDATE OF status ON messages
WHEN
  -- final: read e failed não mudam mais
  OLD.status IN ('read', 'failed', 'received') AND NEW.status <> OLD.status
  -- failed só antes de delivered
  OR (NEW.status = 'failed' AND OLD.status IN ('delivered', 'read'))
  -- no caminho feliz, só pra frente
  OR (NEW.status <> 'failed' AND
      (CASE NEW.status WHEN 'queued' THEN 0 WHEN 'accepted' THEN 1 WHEN 'sent' THEN 2
                       WHEN 'delivered' THEN 3 WHEN 'read' THEN 4 ELSE -1 END)
      <
      (CASE OLD.status WHEN 'queued' THEN 0 WHEN 'accepted' THEN 1 WHEN 'sent' THEN 2
                       WHEN 'delivered' THEN 3 WHEN 'read' THEN 4 ELSE -1 END))
BEGIN
  SELECT RAISE(IGNORE);
END;

-- Espelho dos templates aprovados na Meta (sincronizado pelo Kapso).
-- Fora da janela de 24h, só sai mensagem com template daqui em status 'approved'.
CREATE TABLE wa_templates (
  id           TEXT PRIMARY KEY,
  waba_id      TEXT NOT NULL,
  name         TEXT NOT NULL,
  language     TEXT NOT NULL,
  category     TEXT NOT NULL CHECK (category IN ('marketing', 'utility', 'authentication')),
  status       TEXT NOT NULL CHECK (status IN ('approved', 'pending', 'rejected', 'paused', 'disabled')),
  components   TEXT NOT NULL CHECK (json_valid(components)),
  synced_at    INTEGER NOT NULL,
  UNIQUE (waba_id, name, language)
);

-- Dedupe por evento. O Kapso manda X-Idempotency-Key em toda entrega e repete
-- a mesma chave nos retries (3 tentativas: 10s, 40s, 90s). Cobre o que o
-- unique de messages.external_id não cobre: evento de status, que não cria linha.
-- O Alarm do DO apaga o que tiver mais de 7 dias.
CREATE TABLE webhook_receipts (
  idempotency_key TEXT PRIMARY KEY,
  event           TEXT NOT NULL,     -- X-Webhook-Event, ex.: whatsapp.message.received
  received_at     INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX idx_webhook_receipts_received ON webhook_receipts (received_at);

-- ---------------------------------------------------------------- outbox
-- Efeito externo (envio no Kapso) nunca sai de dentro da transação.
-- A transação grava a mensagem 'queued' + uma linha aqui; o Alarm do DO
-- drena para a Queue; o consumer chama o Kapso e devolve o resultado ao DO.
CREATE TABLE outbox (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL CHECK (kind IN ('send_message', 'sync_templates', 'download_media')),
  ref_id          TEXT NOT NULL,
  payload         TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(payload)),
  attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  next_attempt_at INTEGER NOT NULL,
  last_error      TEXT,
  created_at      INTEGER NOT NULL,
  UNIQUE (kind, ref_id)
);
CREATE INDEX idx_outbox_due ON outbox (next_attempt_at);
