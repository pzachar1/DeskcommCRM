#!/usr/bin/env python3
"""Fase 4 de ponta a ponta: o agente responde o WhatsApp sozinho.

Sobe DOIS servidores de verdade — um Kapso falso (onde a resposta tem de chegar)
e um modelo falso no formato da Messages API (que devolve o texto programado) —
e dirige tudo pela API, como um integrador faria.

O que isto prova e o teste de host não prova: a rodada do agente acontece no
Alarm do Durable Object, com D1, DO, Queue e Alarm de verdade no workerd.

Uso:
    cd worker
    npx wrangler d1 migrations apply crm --local
    cat > .dev.vars <<EOF
    ALLOW_SIGNUP="true"
    KAPSO_API_BASE="http://127.0.0.1:8799"
    KAPSO_API_KEY="chave-de-teste"
    KAPSO_WEBHOOK_SECRET="segredo-de-teste"
    ANTHROPIC_API_BASE="http://127.0.0.1:8798"
    ANTHROPIC_API_KEY="chave-do-modelo"
    EOF
    npx wrangler dev --port 8787 &
    python3 e2e/agente.py http://127.0.0.1:8787
"""

import hashlib
import hmac
import json
import secrets
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8787"
KAPSO_PORT = 8799
MODELO_PORT = 8798
SECRET = b"segredo-de-teste"
passed = 0
tag = secrets.token_hex(3)
PNID = str(int(tag, 16)).rjust(12, "1")
WA_ID = "55119" + str(int(tag, 16) % 10**8).rjust(8, "0")

enviado_ao_kapso = []
pedidos_ao_modelo = []
# o que o modelo falso responde, na ordem; o último valor repete
respostas = ["Atendemos sim, das 8h às 12h no sábado."]


class FakeKapso(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        enviado_ao_kapso.append(body)
        responde(self, 200, {
            "messaging_product": "whatsapp",
            "contacts": [{"input": body.get("to"), "wa_id": body.get("to")}],
            "messages": [{"id": f"wamid.saida-{len(enviado_ao_kapso)}-{tag}"}],
        })

    def log_message(self, *_):
        pass


class FakeModelo(BaseHTTPRequestHandler):
    """Messages API o bastante para o Worker: `content[].text` e `usage`."""

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        pedidos_ao_modelo.append({"path": self.path, "api_key": self.headers.get("x-api-key"), "body": body})
        texto = respostas[min(len(pedidos_ao_modelo), len(respostas)) - 1]
        if texto == "__500__":
            return responde(self, 500, {"error": {"type": "api_error", "message": "deu ruim"}})
        responde(self, 200, {
            "id": f"msg_{len(pedidos_ao_modelo)}",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": texto}],
            "usage": {"input_tokens": 321, "output_tokens": 45},
        })

    def log_message(self, *_):
        pass


def responde(handler, status, corpo):
    raw = json.dumps(corpo).encode()
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(raw)))
    handler.end_headers()
    handler.wfile.write(raw)


for porta, classe in ((KAPSO_PORT, FakeKapso), (MODELO_PORT, FakeModelo)):
    threading.Thread(target=HTTPServer(("127.0.0.1", porta), classe).serve_forever, daemon=True).start()


# ---------------------------------------------------------------- helpers
def call(method, path, body=None, cookie=None, headers=None, raw=None):
    data = raw if raw is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(BASE + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    if cookie:
        req.add_header("Cookie", cookie)
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.loads(r.read() or b"{}"), r.headers
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}"), e.headers


def check(label, cond, detail=""):
    global passed
    if not cond:
        print(f"FALHOU: {label} {detail}")
        sys.exit(1)
    passed += 1
    print(f"ok  {label}")


def wait_for(label, fn, timeout=25):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            check(label, True)
            return last
        time.sleep(0.4)
    check(label, False, f"(esperou {timeout}s) último valor: {last}")


def webhook(event, payload, key=None):
    raw = json.dumps(payload).encode()
    return call("POST", "/webhooks/kapso", raw=raw, headers={
        "X-Webhook-Signature": hmac.new(SECRET, raw, hashlib.sha256).hexdigest(),
        "X-Webhook-Event": event,
        "X-Idempotency-Key": key or secrets.token_hex(8),
        "X-Webhook-Payload-Version": "v2",
    })


def recebida(wamid, texto):
    return {
        "message": {
            "id": wamid, "timestamp": str(int(time.time())), "type": "text", "text": {"body": texto},
            "kapso": {"direction": "inbound", "status": "received", "origin": "cloud_api", "has_media": False, "content": texto},
        },
        "conversation": {
            "id": f"conv-{tag}", "phone_number": "+" + WA_ID, "status": "active", "phone_number_id": PNID,
            "kapso": {"contact_name": "Ana Souza"},
        },
        "is_new_conversation": True,
        "phone_number_id": PNID,
    }


def mensagens(conv):
    _, body, _ = call("GET", f"/api/v1/conversations/{conv}/messages?limit=50", cookie=cookie)
    return body["data"]["items"]


def saidas_do_agente(conv):
    return [m for m in mensagens(conv) if m["sent_via"] == "ai"]


def conversa(conv):
    _, body, _ = call("GET", f"/api/v1/conversations/{conv}", cookie=cookie)
    return body["data"]


def contato_do(conv):
    _, body, _ = call("GET", f"/api/v1/contacts/{conversa(conv)['contact_id']}", cookie=cookie)
    return body["data"]


def rodadas():
    _, body, _ = call("GET", "/api/v1/agent/runs?limit=50", cookie=cookie)
    return body["data"]["items"]


# ---------------------------------------------------------------- conta, número, agente
st, body, h = call("POST", "/api/v1/auth/signup", {
    "email": f"agente+{tag}@exemplo.com", "password": "senha-forte-123",
    "tenant_name": "Clínica Bem Viver", "tenant_slug": f"ag-{tag}",
})
check("signup", st == 201, body)
cookie = h["Set-Cookie"].split(";")[0]

st, body, _ = call("POST", "/api/v1/whatsapp-numbers", {"phone_number_id": PNID, "display_phone": "+5511900000000"}, cookie=cookie)
check("cadastra o número", st == 201, body)

st, body, _ = call("GET", "/api/v1/agent", cookie=cookie)
check("agente nasce desligado", st == 200 and body["data"]["is_enabled"] is False, body)

st, body, _ = call("PUT", "/api/v1/agent", {"is_enabled": True}, cookie=cookie)
check("ligar sem prompt é recusado", st == 422 and body["error"]["code"] == "agent_prompt_required", body)

st, body, _ = call("PUT", "/api/v1/agent", {
    "is_enabled": True,
    "provider": "anthropic",
    "model": "claude-sonnet-5",
    "system_prompt": "Você atende a Clínica Bem Viver pelo WhatsApp. Responda curto.",
    "debounce_ms": 0,
    "handoff_keywords": ["falar com atendente"],
}, cookie=cookie)
check("configura o agente", st == 200 and body["data"]["is_enabled"] is True, body)

# ---------------------------------------------------------------- o caminho normal
st, body, _ = webhook("whatsapp.message.received", recebida(f"wamid.in1-{tag}", "vocês atendem no sábado?"))
check("webhook aceito", st == 200 and body["data"]["queued"] == 1, body)

conv = wait_for("a conversa apareceu", lambda: (call("GET", "/api/v1/conversations", cookie=cookie)[1]["data"]["items"] or [None])[0])["id"]
saida = wait_for("o agente respondeu sozinho", lambda: (saidas_do_agente(conv) or [None])[0])
check("a resposta é a do modelo", saida["body"] == respostas[0], saida)
check("o texto do cliente foi ao modelo",
      any("atendem no sábado" in json.dumps(p["body"], ensure_ascii=False) for p in pedidos_ao_modelo), pedidos_ao_modelo)
check("o prompt do tenant foi como system",
      pedidos_ao_modelo[0]["body"].get("system", "").startswith("Você atende a Clínica"), pedidos_ao_modelo[0]["body"])
check("a chave do modelo vai no header", pedidos_ao_modelo[0]["api_key"] == "chave-do-modelo")
check("a resposta saiu pelo Kapso",
      any(e.get("text", {}).get("body") == respostas[0] for e in enviado_ao_kapso), enviado_ao_kapso)
wait_for("e ficou aceita pelo canal", lambda: (saidas_do_agente(conv) or [{}])[0].get("status") in ("accepted", "sent"))
check("a conversa está com o agente", conversa(conv)["status"] == "ai_handling", conversa(conv))

r = rodadas()[0]
check("a rodada ficou no log com custo",
      r["outcome"] == "replied" and r["tokens_in"] == 321 and r["model"] == "claude-sonnet-5", r)

# ---------------------------------------------------------------- mensagem repetida do Kapso
antes = len(saidas_do_agente(conv))
webhook("whatsapp.message.received", recebida(f"wamid.in1-{tag}", "vocês atendem no sábado?"), key="repetida")
time.sleep(3)
check("entrega repetida não gera segunda resposta", len(saidas_do_agente(conv)) == antes, saidas_do_agente(conv))

# ---------------------------------------------------------------- pedido de atendente
antes = len(saidas_do_agente(conv))
webhook("whatsapp.message.received", recebida(f"wamid.in2-{tag}", "prefiro FALAR COM ATENDENTE"))
wait_for("pedido de atendente passa a conversa para humano", lambda: conversa(conv)["status"] == "human")
check("e não gasta modelo", len(saidas_do_agente(conv)) == antes, saidas_do_agente(conv))
check("com o motivo no log", rodadas()[0]["reason"] == "handoff_keyword", rodadas()[0])

# ---------------------------------------------------------------- humano responde, agente cala
st, body, _ = call("POST", f"/api/v1/conversations/{conv}/messages", {"type": "text", "body": "Oi, Ana! Eu te ajudo por aqui."}, cookie=cookie)
check("o atendente responde pela tela", st == 201, body)
antes = len(saidas_do_agente(conv))
webhook("whatsapp.message.received", recebida(f"wamid.in3-{tag}", "obrigada!"))
time.sleep(3)
check("o agente não fala em cima do atendente", len(saidas_do_agente(conv)) == antes, saidas_do_agente(conv))

# ---------------------------------------------------------------- devolvendo ao agente
respostas.append("De nada! Qualquer coisa, estou aqui.")
st, body, _ = call("POST", f"/api/v1/conversations/{conv}/ai", {}, cookie=cookie)
check("o atendente devolve a conversa ao agente", st == 200 and body["data"]["status"] == "ai_handling", body)
webhook("whatsapp.message.received", recebida(f"wamid.in4-{tag}", "mais uma dúvida: aceitam convênio?"))
wait_for("e ele volta a responder", lambda: len(saidas_do_agente(conv)) > antes)

# ---------------------------------------------------------------- pedido de saída
antes = len(saidas_do_agente(conv))
webhook("whatsapp.message.received", recebida(f"wamid.in5-{tag}", "pare de me mandar mensagem"))
wait_for("quem pede para sair é bloqueado", lambda: contato_do(conv).get("is_blocked"))
check("com o motivo registrado", contato_do(conv).get("blocked_reason") == "opt_out", contato_do(conv))
time.sleep(2)
check("e não recebe mais nenhuma resposta", len(saidas_do_agente(conv)) == antes, saidas_do_agente(conv))

print(f"\n{passed} verificações, todas ok")
