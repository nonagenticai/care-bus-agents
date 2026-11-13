# care-bus-agents

Customer-care agents built on [rig](https://github.com/0xPlaygrounds/rig), decoupled by Redis Streams.

```
interaction ─▶ bus:customer:{area} ─▶ orchestrator ─▶ bus:worker:{area}:{tool} ─▶ worker ─▶ bus:tool:{tool} ─▶ tool-executor ─▶ MCP
     ▲            (request.resolved)        ▲               (work.*)                 ▲        (tool.response)        │
     └──────────── bus:customer:{area} ◀────┴── bus:worker:{area}:results ◀──────────┴──────── bus:tool:{tool} ◀────┘
```

`area` ∈ order-support, logistics, payments, faq. Every publish is mirrored on Pub/Sub `console`.

```sh
docker compose up -d                      # Redis 8 on :6391
cargo test                                # unit + end-to-end over that Redis
cargo build --bins
./target/debug/orchestrator               # CAREBUS_API_URL set: PUBLISHED procedures from carebus-api
for t in crm order delivery faq business-rules procedures; do ./target/debug/worker $t & done
./target/debug/tool-executor              # MCP_URL set: a streamable-HTTP MCP server; else in-process fake tools
./target/debug/tail                       # live view of every bus
./target/debug/interaction                # chat as the customer; uses rig if VLLM_LICENSE_TOKEN is set
```

Env:
- `REDIS_URL`, `CAREBUS_PREFIX`
- intake: `VLLM_LICENSE_TOKEN`, `CAREBUS_LLM_BASE_URL`, `CAREBUS_MODEL`, `SERVICE_ACTOR_ID` (optional: care-bus'
  Keycloak service-account sub, a hyphenated non-nil uuid; malformed stops startup). A platform-model call made for a
  tenant carries `X-Effective-Tenant-Id`, `X-App-Id: care-bus`, `X-Project-Id` (the tenant's tenant-level project:
  its uuid's hex reversed) and `X-Actor-Id` (the person behind it, else `SERVICE_ACTOR_ID`, else omitted), which the
  platform's model router requires in enforce mode. A tenant's own model provider gets none of them.
- tool-executor: `MCP_URL`, `CAREBUS_API_URL`, `CAREBUS_API_TOKEN` (every MCP call is made as the scope's tenant, with that tenant's care-bus licence and strata-rag token from carebus-api `POST /api/internal/mcp-credentials`; no Keycloak client), `TOOL_TIMEOUT_MS` (default 3000), `TOOL_TIMEOUT_MS_FAQ` (default 20000)
- worker: `TOOL_TIMEOUT_MS` (default 3000), `TOOL_TIMEOUT_MS_FAQ` (default 20000)
- orchestrator: `CAREBUS_API_URL`, `CAREBUS_API_TOKEN`

Live tests (`--ignored`): `live_vllm` and `live_mcp` need a real model gateway and MCP server respectively; the
default suite runs against Redis and the in-process fakes.

## License

Apache 2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
