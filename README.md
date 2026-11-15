# care-bus-agents

[![CI](https://github.com/nonagenticai/care-bus-agents/actions/workflows/ci-public.yml/badge.svg)](https://github.com/nonagenticai/care-bus-agents/actions/workflows/ci-public.yml)
[![Release](https://img.shields.io/github/v/release/nonagenticai/care-bus-agents)](https://github.com/nonagenticai/care-bus-agents/releases)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

Customer-care agents built on [rig](https://github.com/0xPlaygrounds/rig), decoupled by
Redis Streams. Each stage — customer intake, orchestration, a per-tool worker, the tool
executor and the MCP server behind it — is a separate process that talks only through the
bus, so a slow or failing tool costs one worker, not the conversation, and any stage scales
by running more replicas.

```
interaction ─▶ bus:customer:{area} ─▶ orchestrator ─▶ bus:worker:{area}:{tool} ─▶ worker ─▶ bus:tool:{tool} ─▶ tool-executor ─▶ MCP
     ▲            (request.resolved)        ▲               (work.*)                 ▲        (tool.response)        │
     └──────────── bus:customer:{area} ◀────┴── bus:worker:{area}:results ◀──────────┴──────── bus:tool:{tool} ◀────┘
```

`area` is one of four: `order-support`, `logistics`, `payments`, `faq`
([`crates/carebus/src/domain.rs`](crates/carebus/src/domain.rs)). Every publish is mirrored
on Pub/Sub `console`.

| | |
|---|---|
| **Runtime** | Rust 1.99.1 (pinned in [`rust-toolchain.toml`](rust-toolchain.toml)); Node.js >= 26 for `web/` |
| **Depends on** | Redis 8.10.1 (the bus); PostgreSQL 18 and a Keycloak realm for `carebus-api`; an MCP server (optional — the tool executor has in-process fakes) |
| **Runs on** | A container ([`Dockerfile.rust`](Dockerfile.rust), [`web/Dockerfile`](web/Dockerfile)) or a host process |

`Dockerfile.rust` builds three images from one file with `--build-arg PACKAGE=` set to
`carebus` (the five bus binaries), `carebus-api` (the API and `researcher`) or
`demo-backends` (fake CRM, order, delivery and FAQ services).

## Quick start

```bash
git clone https://github.com/nonagenticai/care-bus-agents.git
cd care-bus-agents
docker compose up -d       # Redis 8 on :6391 and a demo PostgreSQL 18 on :5442
cargo build --locked
export TEST_DATABASE_URL=postgres://carebus_demo:carebus_demo@127.0.0.1:5442/carebus_demo
cargo test --locked        # unit + end-to-end over the compose services
```

`TEST_DATABASE_URL` is the server the API and backend suites use; each test still creates
its own fresh database inside it. The `--ignored` tests named `live_vllm` and `live_mcp` are
the only ones needing a real model gateway and a real MCP server.

Run the agents:

```bash
./target/debug/orchestrator                       # CAREBUS_API_URL set: procedures from carebus-api
for t in crm order delivery faq business-rules procedures; do ./target/debug/worker $t & done
./target/debug/tool-executor                      # MCP_URL set: a streamable-HTTP MCP server; else in-process fake tools
./target/debug/tail                               # live view of every bus
./target/debug/interaction                        # chat as the customer; uses rig if VLLM_LICENSE_TOKEN is set
```

`docker compose up -d` also starts `carebus-demo-db`, PostgreSQL 18 on `:5442`, for
`demo-backends` and the API's own suite.

## Usage

`carebus-api` binds wherever `CAREBUS_API_BIND` says, and its health route is
unauthenticated:

```console
$ CAREBUS_API_BIND=127.0.0.1:8198 ./target/debug/carebus-api &
$ curl -s http://127.0.0.1:8198/health
{"redis":"ok","db":"ok"}
```

`redis` and `db` each report `ok` or `down` for the bus and the database respectively.

To watch a conversation move through the bus, tail one scope's Pub/Sub mirror:

```bash
./target/debug/tail <tenant>/<job> [stream]
```

It prints one line per publish — stream, topic, correlation id, `from -> to` — and one per
recipient ack. It is lossy by design: an operator's view, not a record.

## Configuration

The binaries read their own variables; the table groups them by the process that reads
them. A required variable that is unset or empty stops the process, naming it.

| Variable | Default | Meaning |
|---|---|---|
| `REDIS_URL` | `redis://127.0.0.1:6391` | the bus |
| `CAREBUS_PREFIX` | empty | stream-key prefix |
| `CAREBUS_API_BIND` | required | `carebus-api` listen address |
| `DATABASE_URL` | required | `carebus-api` owner role; migrations only, then closed |
| `DATABASE_APP_URL` | required | `carebus-api` runtime role (`carebus_app`, subject to row-level security) |
| `KEYCLOAK_ISSUER` | required | `carebus-api` OIDC issuer |
| `KEYCLOAK_AUDIENCE` | required | expected token audience |
| `KEYCLOAK_SERVICE_ONLY_REALMS` | `secure-apps` | realms whose tokens verify only on the service-account doors |
| `CAREBUS_API_CORS_ORIGINS` | empty | comma-separated browser origins; empty sends no CORS headers |
| `CAREBUS_GATES` | unset (gates on) | `off` disables the gates; otherwise `CAREBUS_API_URL` and `CAREBUS_API_TOKEN` are both required |
| `CAREBUS_API_URL` | unset | `carebus-api` base URL, read by the orchestrator, workers and tool executor |
| `CAREBUS_API_TOKEN` | unset | the service token sent with those calls |
| `MCP_URL` | unset | MCP server endpoint (ends in `/mcp`); unset = the tool executor serves in-process fake tools |
| `TOOL_TIMEOUT_MS` | `3000` | per-tool call budget |
| `TOOL_TIMEOUT_MS_FAQ` | `20000` | the `faq` tool's budget (a slower model behind it) |
| `VLLM_LICENSE_TOKEN` | unset | enables the rig-backed model path; unset = rule-based qualifiers |
| `CAREBUS_LLM_BASE_URL` | `https://api.nonagentic.ai/vllm/v1` | model gateway base URL |
| `CAREBUS_MODEL` | `local-llm` | model id sent to that gateway |
| `SERVICE_ACTOR_ID` | unset | care-bus' Keycloak service-account sub, sent as `X-Actor-Id` when no person is behind a call; malformed stops startup |
| `STRATA_RAG_URL`, `STRATA_INGEST_URL` | unset | knowledge and ingest endpoints; setting either also requires `STRATA_TOKENS_TENANT_ID` (or bundle tokens) |
| `CAREBUS_SOURCE_KEY` | unset | 32-byte base64 AES-256-GCM key for source credentials; unset = they can be neither stored nor used |
| `LICENSE_GATE_2_ENABLED` | `false` | the optional tenant licence gate (see [Licensing](#licensing)) |
| `LICENSE_APP_KEY` | `care-bus` | the app key a licence must carry |
| `LICENSE_CACHE_TTL_SEC` | `300` | how long a positive licence verdict is reused |

The SvelteKit console reads its configuration at request time and refuses to start without
all four:

| Variable | Default | Meaning |
|---|---|---|
| `PUBLIC_CAREBUS_API_URL` | required | `carebus-api` base URL |
| `PUBLIC_KEYCLOAK_URL` | required | the realm's base URL |
| `PUBLIC_KEYCLOAK_REALM` | required | realm name |
| `PUBLIC_KEYCLOAK_CLIENT_ID` | required | browser client id |

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI runs the same checks per crate, with `cargo test --doc` and `cargo audit` in the bus
job, and each crate's suite against its own live service: `carebus` against Redis,
`carebus-api` against Redis and PostgreSQL 18, `demo-backends` against PostgreSQL 18.

Web:

```bash
cd web
npm ci
npm run lint        # prettier --check
npm run ci:verify   # svelte-check --fail-on-warnings
npm test            # vitest
npm run build
```

CI is [`.github/workflows/ci-public.yml`](.github/workflows/ci-public.yml): secret scan, a
private-reference scan, one job per crate, the web checks, and image build, smoke test and
GHCR publish when the repository variable `PUBLISH_IMAGE` is `true`.

## Security

Report vulnerabilities privately through the repository's Security tab — see
[`SECURITY.md`](SECURITY.md). Do not open a public issue for a security problem.

## Contributing

Issues and pull requests are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the
ground rules, branch and commit conventions, and the local gate; this repository's own
commands are in **Development** above. Commits carry DCO sign-off (`git commit -s`).

## Licensing

The code in this repository is Apache-2.0 and needs no licence key to build, test or run.
The care-bus API integrates with an **optional** licence gate: when `LICENSE_GATE_2_ENABLED`
is not `true` (the default), the gate returns immediately and no licence request is ever
built. Configured, enforcement is fail-closed: an unreachable licences service denies the
request rather than allowing it.

The licensing stack is public too — [`licenses`](https://github.com/nonagenticai/licenses) (entitlements, plans and tokens) and [`license-authz`](https://github.com/nonagenticai/license-authz) (the Envoy `ext_authz` enforcement point). Neither is required to run this code.

## License

Apache 2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
