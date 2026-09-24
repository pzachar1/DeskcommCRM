# crm-schema

Fase 1 do CRM em Rust + Cloudflare: o modelo de dados. Tira o domínio do
DeskcommCRM (`supabase/baseline.sql`) e reescreve pra dois bancos SQLite,
com WhatsApp pela API oficial via Kapso.

Esta pasta é independente do resto do repositório e foi feita pra virar um repo próprio.

```bash
cargo test                                    # aplica as migrations num SQLite e prova as regras
cargo build --target wasm32-unknown-unknown   # o mesmo crate roda no Worker
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
| Dedupe | `X-Idempotency-Key`, um UUID por evento, repetido nos retries |
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

## Falta verificar antes da fase 2

- Se o SQLite do Durable Object liga `foreign_keys` por padrão. Se não ligar, o runner de
  migration tem que rodar `PRAGMA foreign_keys = ON`. Os testes rodam com FK ligada.
- O runner de migration do DO: aplicar `TENANT_MIGRATIONS` com `version > PRAGMA user_version`
  dentro de `blockConcurrencyWhile` no construtor.
