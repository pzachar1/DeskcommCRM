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
| SQLite do DO, um por tenant | `migrations/tenant/` | contacts, tags, pipelines, stages, leads, lead_activities, lead_links, conversations, messages, wa_templates, outbox |

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
entrada: Kapso ─webhook─> Worker (valida assinatura, lê phone_number_id)
         └─> D1 whatsapp_numbers: tenant ─> Queue ─> DO do tenant
             INSERT messages ... ON CONFLICT (external_id) DO NOTHING

saída:   DO grava messages(queued) + outbox  ─Alarm─> Queue ─> consumer chama Kapso
         └─> DO: status = accepted, external_id = wamid
         status webhooks: sent / delivered / read / failed (o trigger ignora regressão)
```

O tenant de um webhook sai **só** do `phone_number_id` resolvido no D1, nunca de um
campo livre do payload.

## Falta verificar antes da fase 2

- Formato da assinatura e política de retry do webhook do Kapso. A documentação não
  abriu no ambiente em que isto foi escrito.
- Se o SQLite do Durable Object liga `foreign_keys` por padrão. Se não ligar, o runner de
  migration tem que rodar `PRAGMA foreign_keys = ON`. Os testes rodam com FK ligada.
- O runner de migration do DO: aplicar `TENANT_MIGRATIONS` com `version > PRAGMA user_version`
  dentro de `blockConcurrencyWhile` no construtor.
