#!/usr/bin/env python3
"""Fase 3 de ponta a ponta: webhook do Kapso -> Queue -> DO, e envio pelo Alarm.

Sobe um Kapso FALSO (servidor HTTP de verdade) que grava cada envio recebido e
responde como a Cloud API. O Worker roda no `wrangler dev` apontando pra ele.

Uso:
    cd worker
    npx wrangler d1 migrations apply crm --local
    cat > .dev.vars <<EOF
    ALLOW_SIGNUP="true"
    KAPSO_API_BASE="http://127.0.0.1:8799"
    KAPSO_API_KEY="chave-de-teste"
    KAPSO_WEBHOOK_SECRET="segredo-de-teste"
    EOF
    npx wrangler dev --port 8787 &
    python3 e2e/whatsapp.py http://127.0.0.1:8787
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
FAKE_PORT = 8799
API_KEY = "chave-de-teste"
SECRET = b"segredo-de-teste"
passed = 0
tag = secrets.token_hex(3)
PNID = str(int(tag, 16)).rjust(12, "1")          # phone_number_id único por execução
WA_ID = "55119" + str(int(tag, 16) % 10**8).rjust(8, "0")

# ---------------------------------------------------------------- Kapso falso
sent_to_kapso = []


class FakeKapso(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        sent_to_kapso.append({"path": self.path, "api_key": self.headers.get("X-API-Key"), "body": body})
        text = (body.get("text") or {}).get("body")
        if text == "falhar-permanente":
            status, resp = 400, {"error": {"code": 131047, "message": "Re-engagement message"}}
        else:
            status = 200
            resp = {
                "messaging_product": "whatsapp",
                "contacts": [{"input": body.get("to"), "wa_id": body.get("to")}],
                "messages": [{"id": f"wamid.fake-{len(sent_to_kapso)}-{tag}"}],
            }
        raw = json.dumps(resp).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_):
        pass


threading.Thread(target=HTTPServer(("127.0.0.1", FAKE_PORT), FakeKapso).serve_forever, daemon=True).start()


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


def wait_for(label, fn, timeout=20):
    """Queue e Alarm são assíncronos: espera a condição virar verdade."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = fn()
        if last:
            check(label, True)
            return last
        time.sleep(0.4)
    check(label, False, f"(esperou {timeout}s) último valor: {last}")


def webhook(event, payload, key=None, signature=None, batch=False):
    raw = json.dumps(payload).encode()
    sig = signature or hmac.new(SECRET, raw, hashlib.sha256).hexdigest()
    headers = {
        "X-Webhook-Signature": sig,
        "X-Webhook-Event": event,
        "X-Idempotency-Key": key or secrets.token_hex(8),
        "X-Webhook-Payload-Version": "v2",
    }
    if batch:
        headers["X-Webhook-Batch"] = "true"
        headers["X-Batch-Size"] = str(len(payload["data"]))
    return call("POST", "/webhooks/kapso", raw=raw, headers=headers)


def received(wamid, text, ts, wa_id=WA_ID):
    return {
        "message": {
            "id": wamid, "timestamp": str(ts), "type": "text", "text": {"body": text},
            "kapso": {"direction": "inbound", "status": "received", "origin": "cloud_api", "has_media": False, "content": text},
        },
        "conversation": {
            "id": f"conv-{tag}", "phone_number": "+" + wa_id, "status": "active", "phone_number_id": PNID,
            "kapso": {"contact_name": "Ana Souza"},
        },
        "is_new_conversation": True,
        "phone_number_id": PNID,
    }


def status_event(wamid, status, callback=None, origin="cloud_api", errors=None):
    st = {"id": wamid, "status": status, "timestamp": str(int(time.time())), "recipient_id": WA_ID}
    if callback:
        st["biz_opaque_callback_data"] = callback
    if errors:
        st["errors"] = errors
    return {
        "message": {
            "id": wamid, "timestamp": str(int(time.time())), "type": "text", "text": {"body": "x"},
            "kapso": {"direction": "outbound", "status": status, "origin": origin, "has_media": False, "statuses": [st]},
        },
        "conversation": {"id": f"conv-{tag}", "phone_number": "+" + WA_ID, "phone_number_id": PNID},
        "is_new_conversation": False,
        "phone_number_id": PNID,
    }


# ---------------------------------------------------------------- conta e número
st, body, h = call("POST", "/api/v1/auth/signup", {
    "email": f"wa+{tag}@exemplo.com", "password": "senha-forte-123",
    "tenant_name": "Círculo Pallas", "tenant_slug": f"wa-{tag}",
})
check("signup", st == 201, body)
cookie = h["Set-Cookie"].split(";")[0]

now = int(time.time())
st, body, _ = webhook("whatsapp.message.received", received(f"wamid.antes-{tag}", "antes do cadastro", now))
check("número sem tenant: 200 e nada enfileirado", st == 200 and body["data"]["queued"] == 0, body)

st, body, _ = call("POST", "/api/v1/whatsapp-numbers", {"phone_number_id": PNID, "display_phone": "+55 11 90000-0000", "label": "Comercial"}, cookie=cookie)
check("cadastra o número do WhatsApp", st == 201 and body["data"]["display_phone"] == "+5511900000000", body)
st, body, _ = call("POST", "/api/v1/whatsapp-numbers", {"phone_number_id": PNID, "display_phone": "+5511900000000"}, cookie=cookie)
check("mesmo número de novo dá 409", st == 409 and body["error"]["code"] == "phone_number_taken", body)
st, body, _ = call("GET", "/api/v1/whatsapp-numbers", cookie=cookie)
check("lista os números do tenant", st == 200 and [n["phone_number_id"] for n in body["data"]] == [PNID], body)

# ---------------------------------------------------------------- entrada
st, body, _ = webhook("whatsapp.message.received", received("wamid.x", "oi", now), signature="00" * 32)
check("assinatura errada dá 401", st == 401 and body["error"]["code"] == "invalid_signature", body)

key = f"entrega-{tag}"
st, body, _ = webhook("whatsapp.message.received", received(f"wamid.1-{tag}", "oi, quero saber o preço", now), key=key)
check("webhook assinado: 200 e 1 evento na fila", st == 200 and body["data"]["queued"] == 1, body)

inbox = wait_for("mensagem chega na caixa de entrada",
                 lambda: (lambda b: b["data"]["items"] or None)(call("GET", "/api/v1/conversations", cookie=cookie)[1]))
conv = inbox[0]
check("conversa com o contato do WhatsApp", conv["contact_name"] == "Ana Souza" and conv["contact_phone"] == "+" + WA_ID, conv)
check("janela de 24h aberta e 1 não lida", conv["service_window_open"] is True and conv["unread_count"] == 1, conv)
conv_id = conv["id"]


def messages():
    return call("GET", f"/api/v1/conversations/{conv_id}/messages", cookie=cookie)[1]["data"]["items"]


webhook("whatsapp.message.received", received(f"wamid.1-{tag}", "oi, quero saber o preço", now), key=key)
batch = {
    "type": "whatsapp.message.received", "batch": True,
    "data": [received(f"wamid.2-{tag}", "é pra 3 pessoas", now + 1), received(f"wamid.3-{tag}", "tem desconto?", now + 2)],
    "batch_info": {"size": 2, "window_ms": 5000},
}
st, body, _ = webhook("whatsapp.message.received", batch, batch=True)
check("lote com 2 mensagens: 2 eventos na fila", st == 200 and body["data"]["queued"] == 2, body)
wait_for("lote entra, entrega repetida não duplica", lambda: len(messages()) == 3)
check("ordem pela hora da Meta, mais recente primeiro",
      [m["body"] for m in messages()] == ["tem desconto?", "é pra 3 pessoas", "oi, quero saber o preço"], messages())

# ---------------------------------------------------------------- envio
before = len(sent_to_kapso)
st, body, _ = call("POST", f"/api/v1/conversations/{conv_id}/messages", {"type": "text", "body": "custa R$ 1.500"},
                   cookie=cookie, headers={"Idempotency-Key": f"envio-{tag}"})
check("envio entra na fila como queued", st == 201 and body["data"]["status"] == "queued", body)
msg_id = body["data"]["id"]

st, body, _ = call("POST", f"/api/v1/conversations/{conv_id}/messages", {"type": "text", "body": "custa R$ 1.500"},
                   cookie=cookie, headers={"Idempotency-Key": f"envio-{tag}"})
check("mesma Idempotency-Key devolve a mesma mensagem (200)", st == 200 and body["data"]["id"] == msg_id, body)


def msg(mid):
    return next((m for m in messages() if m["id"] == mid), None)


m = wait_for("Alarm envia e grava o wamid", lambda: (lambda x: x if x and x["status"] == "accepted" else None)(msg(msg_id)))
out = [r for r in sent_to_kapso[before:] if r["body"].get("biz_opaque_callback_data") == msg_id]
check("Kapso recebeu UMA chamada pra essa mensagem", len(out) == 1, sent_to_kapso[before:])
req = out[0]
check("chamada no formato da Cloud API",
      req["path"] == f"/{PNID}/messages" and req["api_key"] == API_KEY and req["body"]["to"] == WA_ID
      and req["body"]["type"] == "text" and req["body"]["text"]["body"] == "custa R$ 1.500", req)
check("wamid da resposta do Kapso gravado", m["external_id"].startswith("wamid.fake-"), m)
wamid = m["external_id"]

webhook("whatsapp.message.read", status_event(wamid, "read", callback=msg_id))
wait_for("status read chega pelo webhook", lambda: (msg(msg_id) or {}).get("status") == "read")
st, _, _ = webhook("whatsapp.message.delivered", status_event(wamid, "delivered", callback=msg_id))
time.sleep(2.5)
check("delivered atrasado não regride o read", msg(msg_id)["status"] == "read", msg(msg_id))

st, body, _ = call("POST", f"/api/v1/conversations/{conv_id}/messages", {"type": "text", "body": "falhar-permanente"}, cookie=cookie)
fail_id = body["data"]["id"]
m = wait_for("erro da Meta vira failed", lambda: (lambda x: x if x and x["status"] == "failed" else None)(msg(fail_id)))
check("erro da Meta fica na mensagem", m["error_code"] == "131047" and m["error_message"] == "Re-engagement message", m)

webhook("whatsapp.message.sent", status_event(f"wamid.app-{tag}", "sent", origin="business_app"))
m = wait_for("mensagem mandada pelo app do WhatsApp Business aparece",
             lambda: next((x for x in messages() if x["external_id"] == f"wamid.app-{tag}"), None))
check("app do celular gravado como external_device", m["sent_via"] == "external_device" and m["direction"] == "outbound", m)

st, body, _ = call("POST", f"/api/v1/conversations/{conv_id}/read", {}, cookie=cookie)
check("marcar como lida zera não lidas", st == 200 and body["data"]["unread_count"] == 0, body)

# ---------------------------------------------------------------- falar primeiro (fora da janela)
st, body, _ = call("POST", "/api/v1/contacts", {"name": "Bia", "phone": "+351 912 345 678"}, cookie=cookie)
bia = body["data"]["id"]
st, body, _ = call("POST", "/api/v1/conversations", {"contact_id": bia, "phone_number_id": "999999"}, cookie=cookie)
check("número de outro tenant ou inexistente dá 422", st == 422 and body["error"]["code"] == "unknown_phone_number", body)
st, body, _ = call("POST", "/api/v1/conversations", {"contact_id": bia, "phone_number_id": PNID}, cookie=cookie)
check("abre conversa com contato novo", st == 201 and body["data"]["contact_id"] == bia, body)
bia_conv = body["data"]["id"]
st, body, _ = call("POST", f"/api/v1/conversations/{bia_conv}/messages", {"type": "text", "body": "oi"}, cookie=cookie)
check("texto fora da janela de 24h dá 422", st == 422 and body["error"]["code"] == "service_window_closed", body)
before = len(sent_to_kapso)
st, body, _ = call("POST", f"/api/v1/conversations/{bia_conv}/messages",
                   {"type": "template", "name": "primeiro_contato", "language": "pt_PT",
                    "components": [{"type": "body", "parameters": [{"type": "text", "text": "Bia"}]}]}, cookie=cookie)
check("template fora da janela entra na fila", st == 201 and body["data"]["type"] == "template", body)
wait_for("template chega no Kapso", lambda: any(r["body"].get("type") == "template" for r in sent_to_kapso[before:]))
req = next(r for r in sent_to_kapso[before:] if r["body"].get("type") == "template")
check("template com nome, idioma e parâmetros",
      req["body"]["to"] == "351912345678" and req["body"]["template"]["name"] == "primeiro_contato"
      and req["body"]["template"]["language"] == {"code": "pt_PT"}
      and req["body"]["template"]["components"][0]["parameters"][0]["text"] == "Bia", req)

# ---------------------------------------------------------------- isolamento
st, body, h = call("POST", "/api/v1/auth/signup", {
    "email": f"outro+{tag}@exemplo.com", "password": "senha-forte-123",
    "tenant_name": "Outra empresa", "tenant_slug": f"outra-{tag}",
})
other = h["Set-Cookie"].split(";")[0]
st, body, _ = call("GET", "/api/v1/conversations", cookie=other)
check("outro tenant não vê as conversas", st == 200 and body["data"]["items"] == [], body)
st, body, _ = call("GET", f"/api/v1/conversations/{conv_id}", cookie=other)
check("outro tenant não acha a conversa pelo id", st == 404, body)
st, body, _ = call("POST", "/api/v1/whatsapp-numbers", {"phone_number_id": PNID, "display_phone": "+5511900000000"}, cookie=other)
check("outro tenant não toma o número", st == 409, body)
st, body, _ = call("POST", "/internal/kapso/events", {"event": "x", "key": "k", "payload": {}}, cookie=cookie)
check("rota interna do DO não é alcançável de fora", st == 404, body)

print(f"\n{passed} verificações passaram")
