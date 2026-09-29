#!/usr/bin/env python3
"""Teste de ponta a ponta contra o Worker rodando (`wrangler dev`).

Uso:
    cd worker
    npx wrangler d1 migrations apply crm --local
    echo 'ALLOW_SIGNUP="true"' > .dev.vars
    npx wrangler dev --port 8787 &
    python3 e2e/smoke.py http://127.0.0.1:8787

Cria uma conta nova a cada execução (e-mail e slug com sufixo aleatório),
então pode rodar quantas vezes quiser no mesmo banco local.
"""

import json
import secrets
import sys
import urllib.error
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8787"
passed = 0


def call(method, path, body=None, cookie=None, headers=None, raw_body=None, content_type="application/json"):
    data = raw_body if raw_body is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(BASE + path, data=data, method=method)
    if data is not None:
        req.add_header("Content-Type", content_type)
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


def session_from(headers):
    raw = headers.get("Set-Cookie", "")
    return raw.split(";")[0]


tag = secrets.token_hex(3)
email, password = f"paty+{tag}@exemplo.com", "senha-forte-123"

# ---------------------------------------------------------------- conta
st, body, h = call("POST", "/api/v1/auth/signup", {
    "email": email, "password": password, "name": "Paty",
    "tenant_name": "Círculo Pallas", "tenant_slug": f"pallas-{tag}",
})
check("signup cria conta", st == 201, body)
cookie = session_from(h)
check("cookie de sessão é HttpOnly + SameSite=Strict", "HttpOnly" in h["Set-Cookie"] and "SameSite=Strict" in h["Set-Cookie"])
tenant_id = body["data"]["tenant_id"]
user_id = body["data"]["user_id"]

st, body, _ = call("POST", "/api/v1/auth/signup", {
    "email": email, "password": password, "tenant_name": "X", "tenant_slug": f"outro-{tag}",
})
check("e-mail repetido dá 409", st == 409 and body["error"]["code"] == "already_exists", body)

st, body, _ = call("POST", "/api/v1/auth/signup", {
    "email": f"b{tag}@x.com", "password": "curta", "tenant_name": "X", "tenant_slug": f"b-{tag}",
})
check("senha curta dá 422", st == 422 and body["error"]["code"] == "weak_password", body)

st, body, _ = call("GET", "/api/v1/me", cookie=cookie)
check("me devolve o tenant como admin", st == 200 and body["data"]["tenants"][0]["role"] == "admin", body)

# ---------------------------------------------------------------- auth negativa
st, body, _ = call("GET", "/api/v1/contacts")
check("sem cookie dá 401", st == 401, body)

st, body, _ = call("GET", "/api/v1/contacts", cookie=cookie, headers={"X-Tenant-Id": "tenant-de-outro"})
check("tenant alheio dá 403", st == 403 and body["error"]["code"] == "forbidden_tenant", body)

st, body, _ = call("POST", "/api/v1/contacts", cookie=cookie, raw_body=b"name=x", content_type="application/x-www-form-urlencoded")
check("POST de formulário (CSRF) dá 415", st == 415, body)

st, body, _ = call("POST", "/api/v1/auth/login", {"email": email, "password": "errada-errada"})
check("senha errada dá 401", st == 401 and body["error"]["code"] == "invalid_credentials", body)

st, body, h = call("POST", "/api/v1/auth/login", {"email": email, "password": password})
check("login certo abre sessão nova", st == 200 and "crm_session=" in h.get("Set-Cookie", ""), body)

# ---------------------------------------------------------------- funil semeado
st, body, _ = call("GET", "/api/v1/pipelines", cookie=cookie)
check("conta nova já tem o funil Vendas", st == 200 and body["data"][0]["slug"] == "vendas", body)
pipeline = body["data"][0]
stages = {s["slug"]: s["id"] for s in pipeline["stages"]}
check("funil tem as 5 etapas padrão", list(stages) == ["novo", "qualificado", "proposta", "ganho", "perdido"], stages)

# ---------------------------------------------------------------- contato
st, body, _ = call("POST", "/api/v1/contacts", {"name": "Ana", "phone": "+55 (11) 98765-4321"}, cookie=cookie)
check("cria contato com telefone normalizado", st == 201 and body["data"]["phone_e164"] == "+5511987654321", body)
contact = body["data"]

st, body, _ = call("POST", "/api/v1/contacts", {"phone": "+5511987654321"}, cookie=cookie)
check("telefone repetido dá 409", st == 409, body)

st, body, _ = call("POST", "/api/v1/contacts", {"phone": "11987654321"}, cookie=cookie)
check("telefone sem DDI dá 422", st == 422 and body["error"]["code"] == "invalid_phone", body)

st, body, _ = call("GET", "/api/v1/contacts?q=Ana", cookie=cookie)
check("busca contato", st == 200 and len(body["data"]["items"]) == 1, body)

# ---------------------------------------------------------------- leads e kanban
ids = []
for title in ["A", "B", "C"]:
    st, body, _ = call("POST", "/api/v1/leads", {"title": title, "contact_id": contact["id"], "value_cents": 150000}, cookie=cookie)
    check(f"cria lead {title} na primeira etapa", st == 201 and body["data"]["stage_id"] == stages["novo"], body)
    ids.append(body["data"]["id"])

def column(slug):
    _, b, _ = call("GET", f"/api/v1/pipelines/{pipeline['id']}/board", cookie=cookie)
    col = next(c for c in b["data"]["columns"] if c["stage"]["slug"] == slug)
    return [l["title"] for l in col["leads"]]

check("board mostra A, B, C em ordem", column("novo") == ["A", "B", "C"], column("novo"))

st, body, _ = call("POST", f"/api/v1/leads/{ids[2]}/move", {"stage_id": stages["novo"], "next_lead_id": ids[0]}, cookie=cookie)
check("move C pro topo", st == 200 and column("novo") == ["C", "A", "B"], column("novo"))

st, body, _ = call("POST", f"/api/v1/leads/{ids[0]}/move", {"stage_id": stages["perdido"]}, cookie=cookie)
check("perda sem motivo dá 422", st == 422 and body["error"]["code"] == "lost_reason_required", body)
check("movimento recusado não mexeu no board", column("novo") == ["C", "A", "B"], column("novo"))

st, body, _ = call("POST", f"/api/v1/leads/{ids[0]}/move", {"stage_id": stages["ganho"]}, cookie=cookie)
check("ganho fecha o lead", st == 200 and body["data"]["status"] == "won" and body["data"]["closed_at"], body)

st, body, _ = call("POST", f"/api/v1/leads/{ids[0]}/notes", {"body": "fechou em 3x"}, cookie=cookie)
check("nota entra na timeline", st == 201 and body["data"][0]["type"] == "note_added", body)

st, body, _ = call("GET", f"/api/v1/leads/{ids[0]}/activities", cookie=cookie)
kinds = [a["type"] for a in body["data"]]
check("timeline completa, mais recente primeiro",
      kinds == ["note_added", "status_changed", "stage_changed", "lead_created"], kinds)
check("atividade registra quem fez", all(a["performed_by"] == user_id for a in body["data"]), body["data"])

st, body, _ = call("POST", f"/api/v1/leads/{ids[1]}/move", {"stage_id": stages["novo"]},
                   cookie=cookie, headers={"X-Actor-User-Id": "forjado"})
_, tl, _ = call("GET", f"/api/v1/leads/{ids[1]}/activities", cookie=cookie)
check("header X-Actor-User-Id do cliente é ignorado", all(a["performed_by"] != "forjado" for a in tl["data"]), tl)

st, body, _ = call("PATCH", f"/api/v1/leads/{ids[1]}", {"currency": "real"}, cookie=cookie)
check("moeda inválida dá 422", st == 422, body)

st, body, _ = call("GET", "/api/v1/leads/nao-existe", cookie=cookie)
check("lead inexistente dá 404", st == 404, body)

# ---------------------------------------------------------------- atomicidade no DO
# O funil e a 1ª etapa são gravados antes da 2ª etapa falhar. Se o transactionSync
# não desfizesse, o funil "meio criado" apareceria na listagem.
st, body, _ = call("POST", "/api/v1/pipelines", {
    "name": "Quebrado", "slug": f"quebrado-{tag}",
    "stages": [{"name": "Ok", "slug": "ok"}, {"name": "Ruim", "slug": "Com Espaço"}],
}, cookie=cookie)
check("funil com etapa inválida dá 422", st == 422 and body["error"]["code"] == "invalid_slug", body)
_, body, _ = call("GET", "/api/v1/pipelines", cookie=cookie)
check("rollback: funil meio criado não ficou no banco", [p["slug"] for p in body["data"]] == ["vendas"], body)

# ---------------------------------------------------------------- logout
st, _, h = call("POST", "/api/v1/auth/logout", {}, cookie=cookie)
check("logout", st == 200)
st, _, _ = call("GET", "/api/v1/contacts", cookie=cookie)
check("sessão morta depois do logout", st == 401)

print(f"\n{passed} verificações passaram")
