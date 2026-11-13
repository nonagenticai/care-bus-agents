# demo-backends

The fake CRM, order system, carrier tracking and payment provider for care-bus demos.

```bash
export DEMO_BACKENDS_TOKEN="$(openssl rand -hex 16)"   # any non-empty secret
docker compose up -d carebus-demo-db
cargo build -p demo-backends
DATABASE_URL=postgres://carebus_demo:carebus_demo@127.0.0.1:5442/carebus_demo \
./target/debug/demo-backends     # listens on :8096
curl -s localhost:8096/health
curl -s -H "Authorization: Bearer $DEMO_BACKENDS_TOKEN" localhost:8096/api/customers/C-10482/start-data | jq .
curl -s -X POST -H "Authorization: Bearer $DEMO_BACKENDS_TOKEN" localhost:8096/api/admin/reset | jq .
```

| Env | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | — (required) | DB `carebus_demo` |
| `DEMO_BACKENDS_TOKEN` | — (this or the list is required) | bearer token for every `/api` route |
| `DEMO_BACKENDS_TOKENS` | — | comma list of further accepted bearer tokens, one per calling service |
| `DEMO_BACKENDS_BIND` | `0.0.0.0:8096` | listen address |
| `DEMO_TODAY` | `2026-10-15` | "today" for rules and start-data |
| `DEMO_SEED` | `20261015` | seed used for reset without a body and for the first boot |
| `DEMO_FAULT_TIMEOUT_MS` | `3500` | how long a `timeout` fault stalls (past the workers' 3 s timeout) |
