-- D1 global: só o que precisa ser visto ACIMA de um tenant.
-- Dado de negócio (contato, lead, conversa) mora no Durable Object do tenant.
--
-- Convenções:
--   ids      TEXT, UUIDv7 (ordena por tempo no B-tree do SQLite)
--   tempo    INTEGER, milissegundos desde epoch, UTC
--   booleano INTEGER 0/1 com CHECK

CREATE TABLE tenants (
  id          TEXT PRIMARY KEY,
  slug        TEXT NOT NULL UNIQUE CHECK (slug GLOB '[a-z0-9]*' AND length(slug) BETWEEN 2 AND 40),
  name        TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended', 'closed')),
  -- plano Kapso Platform: cliente final ligado ao seu projeto Kapso
  kapso_customer_id TEXT UNIQUE,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE users (
  id            TEXT PRIMARY KEY,
  email         TEXT NOT NULL UNIQUE COLLATE NOCASE,
  name          TEXT,
  password_hash TEXT,            -- argon2id; NULL quando entra só por link mágico
  is_platform_admin INTEGER NOT NULL DEFAULT 0 CHECK (is_platform_admin IN (0, 1)),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- viewer(1) < agent(2) < manager(3) < admin(4), mesmo RBAC do Deskcomm
CREATE TABLE memberships (
  tenant_id  TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role       TEXT NOT NULL CHECK (role IN ('viewer', 'agent', 'manager', 'admin')),
  created_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id)
);
CREATE INDEX idx_memberships_user ON memberships (user_id);

-- Sessão de navegador. O cookie leva o token; aqui só o SHA-256 dele.
CREATE TABLE sessions (
  token_sha256 TEXT PRIMARY KEY,
  user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at   INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions (user_id);
CREATE INDEX idx_sessions_expires ON sessions (expires_at);

-- Token de API para integração (Authorization: Bearer ...). Plaintext mostrado uma vez.
CREATE TABLE api_tokens (
  id           TEXT PRIMARY KEY,
  tenant_id    TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  name         TEXT NOT NULL,
  token_sha256 TEXT NOT NULL UNIQUE,
  prefix       TEXT NOT NULL,     -- primeiros caracteres, para a tela listar sem revelar
  role         TEXT NOT NULL CHECK (role IN ('viewer', 'agent', 'manager', 'admin')),
  created_by   TEXT REFERENCES users(id) ON DELETE SET NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER,
  created_at   INTEGER NOT NULL
);
CREATE INDEX idx_api_tokens_tenant ON api_tokens (tenant_id);

-- Roteamento do webhook do Kapso: phone_number_id da Meta -> tenant.
-- É a ÚNICA fonte confiável de tenant num webhook; nunca ler tenant do payload.
CREATE TABLE whatsapp_numbers (
  phone_number_id TEXT PRIMARY KEY,   -- id da Meta, vem em todo evento
  tenant_id       TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
  display_phone   TEXT NOT NULL,      -- E.164, só para exibir
  waba_id         TEXT,
  label           TEXT,
  status          TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disconnected')),
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_whatsapp_numbers_tenant ON whatsapp_numbers (tenant_id);
