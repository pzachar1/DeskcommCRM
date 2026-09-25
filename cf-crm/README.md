# crm-schema / crm-core / crm-worker

CRM em Rust + Cloudflare, construído a partir do domínio do DeskcommCRM
(`supabase/baseline.sql`), com WhatsApp pela API oficial via Kapso.
Esta pasta é independente do resto do repositório e foi feita pra virar um repo próprio.

| Crate | Pasta | O que tem |
|---|---|---|
| `crm-schema` | `.` | migrations (D1 e DO), structs do modelo, contrato do webhook do Kapso |
| `crm-core` | `core/` | regras de negócio: contatos, funis, leads, kanban, timeline. Sem runtime: o banco entra por um trait |
| `crm-worker` | `worker/` | Worker de entrada (login, sessão, tenant, papel) + Durable Object por tenant |

```bash
cargo test                                                 # schema + core, SQLite de verdade no host
cargo check -p crm-worker --target wasm32-unknown-unknown  # o worker só compila pra wasm
```

### Rodar o worker local (D1 + Durable Object no workerd)

```bash
cd worker
npx wrangler d1 migrations apply crm --local
echo 'ALLOW_SIGNUP="true"' > .dev.vars
npx wrangler dev --port 8787
python3 e2e/smoke.py http://127.0.0.1:8787   # 34 verificações pela API
```

## Onde mora cada coisa

| Banco | Arquivo | Conteúdo |
|---|---|---|
| D1 global | `migrations/global/` | tenants, users, memberships, sessions, api_tokens, whatsapp_numbers |
| SQLite do DO, um por tenant | `migrations/tenant/` | contacts, tags, pipelines, stages, leads, lead_activities, lead_links, conversations, messages, wa_templates, webhook_receipts, outbox |

O D1 guarda o que precisa ser consultado acima de um tenant: login, permissão e
de qual tenant é cada número de WhatsApp. O resto fica no DO do tenant
(`idFromName(tenant_id)`). Por isso não existe `organization_id` nem RLS: um DO
não consegue ler o banco de outro.

## Decisões que mudam em relação ao Deskcomm

- **Ordem do kanban em `TEXT`, não em número.** O Deskcomm usa `numeric` do Postgres, que
  tem precisão arbitrária. No SQLite seria `REAL`: depois de umas 50 inserções no mesmo
  vão, o ponto médio para de mudar e dois cards empatam. Chave de fractional indexing
  (`"a0"`, `"a0V"`...) resolve e ordena por comparação de texto.
- **A etapa pertence ao funil do lead, e quem garante é o banco.** A FK é composta,
  `(stage_id, pipeline_id)`. No Deskcomm isso era só convenção.
- **`wa_id` separado de `phone_e164`.** No Brasil a Meta pode mandar o `wa_id` de um número
  antigo sem o nono dígito. Buscar o contato pelo telefone criaria uma duplicata.
- **Status da mensagem só anda pra frente.** Webhook de status chega fora de ordem.
  O trigger `trg_messages_status_monotonic` descarta a regressão, e o teste
  `status_rule_matches_trigger` confere que ele e `MessageStatus::can_transition_to`
  concordam em todos os pares de status.
- **A janela de 24h é calculada, não gravada.** `service_window_open(last_inbound_at, now)`.
- **Contato nunca some em cascata.** Tudo que aponta pra `contacts` usa `RESTRICT`. LGPD
  anonimiza, como no Deskcomm.
- **Tags viraram tabela.** O SQLite não tem `text[]`, então ficam `tags` + `contact_tags` + `lead_tags`.
- **Nenhum efeito externo sai de dentro da transação.** A mensagem é gravada como `queued`
  junto com uma linha em `outbox`, na mesma transação.

## Fluxo com o Kapso

```
entrada: Kapso ─webhook─> Worker
           1. verify_signature(secret, corpo cru, X-Webhook-Signature)  -> 401 se falhar
           2. phone_number_id (topo do payload v2) -> D1 whatsapp_numbers -> tenant
           3. Queue.send(evento) e responde 200
         Queue ─> DO do tenant
           INSERT webhook_receipts (X-Idempotency-Key) ON CONFLICT DO NOTHING -> se já existia, para
           INSERT messages ... ON CONFLICT (external_id) DO NOTHING

saída:   DO grava messages(queued) + outbox  ─Alarm─> Queue ─> consumer chama Kapso
         └─> DO: status = accepted, external_id = wamid
         eventos de status: sent / delivered / read / failed (o trigger ignora regressão)
```

O tenant de um webhook sai **só** do `phone_number_id` resolvido no D1, nunca de um
campo livre do payload.

### Contrato do webhook (payload v2)

Fonte: [Webhooks overview](https://docs.kapso.ai/docs/platform/webhooks/overview) e
[Webhook security](https://docs.kapso.ai/docs/platform/webhooks/security) e
[Delivery](https://docs.kapso.ai/docs/platform/webhooks/advanced). Constantes e
verificação em `src/kapso.rs`.

| O quê | Valor |
|---|---|
| Assinatura | `X-Webhook-Signature`, HMAC-SHA256 do corpo cru, em hex |
| Dedupe | `X-Idempotency-Key`, um UUID por entrega, repetido nos retries da mesma entrega |
| Evento | `X-Webhook-Event`, ex.: `whatsapp.message.received` |
| Lote | `X-Webhook-Batch: true` + `X-Batch-Size`; eventos em `data[]` (janela 1 a 60s, até 100) |
| Retry | 3 tentativas contando a primeira (agora, +10s, +40s): desiste em ~50s |
| Resposta | 200 em até 10s |
| Pausa automática | em 15 min, ≥40 entregas, ≥10 falhas e ≥85% de falha: webhook desativado até religar no painel |
| Ordem | por conversa, com número de sequência; depois de 30s entrega fora de ordem |
| Origem | `message.kapso.origin`: `cloud_api`, `business_app`, `history_sync` |

**Consequência do retry curto:** se o Worker cair por mais de ~50s, o evento se perde.
E se cair por mais tempo com tráfego, o Kapso pausa o webhook e para de entregar tudo.
Falta um caminho de reconciliação (Cron Trigger que busca na API do Kapso as mensagens
recentes e reinsere, e a idempotência por `external_id` absorve o que já existia).

### O que isso decide

- **O dedupe de verdade é o `external_id` (wamid), não o `X-Idempotency-Key`.** A chave é
  do header, então vale por entrega. Quando um lote falha nas 3 tentativas, o Kapso
  reenvia as mensagens uma a uma, cada uma com chave nova. A mesma mensagem chega de
  novo com outra chave, e quem segura é o `uq_messages_external`. `webhook_receipts`
  continua servindo pra evento de status, que não cria linha.
- **Com buffering ligado, toda entrega vem em formato de lote**, mesmo com uma mensagem só.
  O parser decide pelo campo `batch`, nunca pela forma do JSON.
- **Ordem de chegada não é ordem de conversa.** A tela ordena por `sent_at` (timestamp da
  Meta), então mensagem atrasada cai no lugar certo.
- **Origem → `sent_via`:** `business_app` vira `external_device` (sua equipe mandou pelo
  app); `cloud_api` com direção `outbound` é eco de envio nosso, e o wamid já existe;
  `history_sync` só entra em importação, nunca dispara agente nem automação.
- **A assinatura é do corpo cru.** O exemplo "production setup" da doc verifica depois de
  re-serializar `data`, e isso quebra a assinatura. Aqui o Worker verifica antes do parse.

## API (fase 2)

Toda resposta: `{ data }` ou `{ error: { code, message } }`, com `X-Request-Id`.
Sessão por cookie `crm_session` (HttpOnly, Secure, SameSite=Strict). Quem pertence a
mais de um tenant manda `X-Tenant-Id`. Escrita exige `Content-Type: application/json`,
o que barra POST de formulário vindo de outro site.

| Rota | Papel mínimo |
|---|---|
| `POST /api/v1/auth/signup` · `login` · `logout`, `GET /api/v1/me` | público / sessão |
| `GET /api/v1/contacts?q=&limit=&cursor=`, `GET /api/v1/contacts/{id}` | viewer |
| `POST /api/v1/contacts`, `PATCH /api/v1/contacts/{id}` | agent |
| `GET /api/v1/pipelines`, `/{id}`, `/{id}/board` | viewer |
| `POST /api/v1/pipelines`, `POST /api/v1/pipelines/{id}/stages` | manager |
| `POST /api/v1/leads`, `PATCH /api/v1/leads/{id}` | agent |
| `POST /api/v1/leads/{id}/move` `{ stage_id, prev_lead_id?, next_lead_id?, lost_reason? }` | agent |
| `POST /api/v1/leads/{id}/notes`, `GET /api/v1/leads/{id}/activities` | agent / viewer |

Conta nova já nasce com o funil "Vendas" (novo → qualificado → proposta → ganho / perdido).

## Como a fase 2 funciona por dentro

- **Migrations do tenant rodam no construtor do DO.** O SQL do DO é síncrono, então
  terminam antes de qualquer requisição chegar. Cada versão aplicada fica em `_migrations`.
  Se uma falhar, o DO responde 500 em vez de operar com banco pela metade.
- **Chave estrangeira já vem ligada** no D1 e no SQLite do DO (workerd), então o runner
  não precisa de `PRAGMA`. Os testes no host ligam explicitamente pra ficar igual.
- **Atomicidade:** o `sql.exec` do DO não aceita `BEGIN`/`SAVEPOINT`, e o `workers-rs`
  0.8.6 não expõe `transactionSync`. O `DoDb::atomic` chama `ctx.storage.transactionSync`
  pelo objeto JS e transforma erro do core em exceção, pro runtime desfazer.
  O `smoke.py` prova isso: um funil que falha na 2ª etapa não deixa resto. Com o
  `transactionSync` desligado de propósito, esse teste quebra.
- **Kanban:** chave de fractional indexing completa (parte inteira + fração, algoritmo do
  Greenspan). Lead novo vai pro fim da coluna, e 10 mil inserções no fim dão chave de 4
  caracteres. Mover um card grava uma linha só.
- **Timeline:** toda mutação de lead grava em `lead_activities` na mesma transação e
  atualiza `last_activity_at`. Reordenar dentro da mesma coluna não gera atividade.
- **Senha:** argon2id com os parâmetros padrão do crate (19 MiB, 2 passadas). No `wrangler dev`
  o login levou uns 50 ms, acima do limite de CPU do plano Free dos Workers (10 ms):
  em produção, precisa do plano Paid. E-mail que não existe custa o mesmo hash, pra não
  revelar quem tem conta.
- **O Worker monta a requisição interna do zero.** Nenhum header do cliente chega ao DO,
  então não dá pra forjar `X-Actor-User-Id` (o `smoke.py` testa).

## Ainda não tem

- Limite de tentativas no login (Rate Limiting binding do Workers)
- Token de API: a tabela `api_tokens` existe, falta a rota que cria e a leitura do `Bearer`
- Convite de usuário e troca de papel
- Log de auditoria das mutações
- Cursor de paginação assinado (hoje é `created_at.id` em texto)
- Webhook do Kapso e envio (fase 3) e a interface em Dioxus
