# carebus-api

HTTP + SSE backend for the care-bus console and admin. Routes are `/api/<resource>` (no version prefix); `GET /health` is
unauthenticated. `GET /api/overview` returns today/week-ago case counters, per-dependency health and the attention list.
The route list is in `src/routes/` (one module per page); the web console's fixtures under `web/src/lib/api/fixtures/` are
written by this crate's tests.

## Configuration (environment variables, by name)

Required:

- `CAREBUS_API_BIND` - listen address (`host:port`).
- `DATABASE_URL` - this service's own Postgres 18 database (`carebus`, role `carebus`).
- `REDIS_URL` - the message bus.
- `KEYCLOAK_ISSUER`, `KEYCLOAK_AUDIENCE` - JWT issuer and audience; signatures are verified against the issuer's JWKS.

Optional:

- `CAREBUS_PREFIX` - Redis key prefix (empty in production).
- `CAREBUS_API_SERVICE_TOKENS` - comma-separated read-only service bearers (from SOPS); role `care-viewer`, never admin.
- `CAREBUS_API_CORS_ORIGINS` - comma-separated browser origins allowed by CORS; empty means no CORS layer.
- `STRATA_RAG_URL`, `STRATA_RAG_TOKEN` - knowledge proxy upstream.
- `STRATA_INGEST_URL`, `STRATA_INGEST_TOKEN` - strata_ingest (bearer; the token decides the tenant). Source re-sync and
  the ingest jobs page; unset or unreachable answers 503 `upstream_unavailable`.
- `STRATA_TOKENS_TENANT_ID` - the tenant the static strata tokens are pinned to. A strata call for any other tenant is
  409 `strata_not_provisioned_for_tenant` and is not sent. Required once either strata URL is set, unless bundle
  tokens are on.
- `CAREBUS_STRATA_BUNDLE_TOKENS=true` - every strata call carries a short-lived licence token minted for the request's
  tenant (`POST {LICENSE_ADMIN_BASE_URL}/internal/tenants/{tenant}/bundle-token?app=strata-rag|strata-ingest`), and
  the static tokens are never sent. The tokens are cached per (tenant, app) until shortly before they expire. Minted
  as the client-credentials SA `LICENSE_BUNDLE_MINTER_CLIENT_ID` (default `care-bus-bundle-minter`, secure-apps,
  realm role `bundle-token-minter`) with `LICENSE_BUNDLE_MINTER_CLIENT_SECRET`, at `LICENSE_SA_TOKEN_URL`;
  `CAREBUS_STRATA_BUNDLE_TOKEN_TTL_MINUTES` sets the lifetime (1 to 60, default 15). A tenant without the licence gets
  409 `strata_not_licensed_for_tenant`. If licences refuses the minter (403/409), the call gets 503
  `strata_licence_refused`. Off (the default) keeps the static behaviour exactly.
- `CAREBUS_TENANT_BUNDLE_RECONCILE=true` - the signup tick registers every active tenant in licences and ensures its
  care-bus licence set once (recorded in `tenant_bundle_reconcile`, retried with backoff), so bundle tokens can be
  minted for tenants older than early access. Default off. Needs the `LICENSE_*` issuing settings.
- `SIGNUP_BASE_TENANT_ID` - the tenant whose `SIGNUP_JOB_KEY` job holds the base release a new early-access
  workspace starts on (default the bootstrap tenant).
- `CAREBUS_SOURCE_KEY` - 32 random bytes, standard base64 (`openssl rand -base64 32`), from a SOPS secret. Encrypts
  source credentials (AES-256-GCM). Unset: credentials can be neither saved nor used (503 `not_configured`); a
  malformed value stops startup. Losing or rotating it makes stored credentials unreadable (409
  `credentials_unreadable`) until they are entered again.
- `DEMO_BACKENDS_URL`, `DEMO_BACKENDS_TOKEN` - demo proxy upstream.
- `MCP_URL` - the nonagenticai MCP server (tool list; the overview probes `{MCP_URL without trailing /mcp}/api/health`).
  Every MCP request is made as one bus tenant: `X-Tenant-Id: Bearer <the tenant's care-bus licence>` (tenant-level,
  subject `svc:<tenant>:care-bus`, minted by licences `bundle-token?app=care-bus&bundle=care-bus`) and
  `X-Strata-Token: <the tenant's strata-rag token>`, both minted by the bundle minter above, so MCP needs
  `CAREBUS_STRATA_BUNDLE_TOKENS=true`. No Keycloak client. The agents (tool-executor, researcher) fetch the same pair
  from `POST /api/internal/mcp-credentials {tenant_id, fresh?}` with their service token (`CAREBUS_API_TOKEN`), answered
  `{tenant_id, licence_token, strata_token, expires_at, expires_in}` (`no-store`); a person's token is 403, a tenant
  without an active scope 404, a tenant without the licence 409 `not_licensed_for_tenant`, no minter 503
  `mcp_credentials_unconfigured`.
- `VLLM_LICENSE_TOKEN`, `CAREBUS_LLM_BASE_URL`, `CAREBUS_MODEL` - LLM qualifier for chat; keyword rules when unset.
- `SERVICE_ACTOR_ID` - care-bus' Keycloak service-account sub (hyphenated, non-nil uuid; malformed stops startup;
  unset = no actor). Set it on carebus-api, interaction and researcher. Platform-model calls (chat qualifier, research
  model) made for a tenant send `X-Effective-Tenant-Id`, `X-App-Id: care-bus`, `X-Project-Id` (the tenant-level
  project: the tenant uuid's 32 hex digits reversed) and `X-Actor-Id` (a research run's creator when it is a uuid
  subject, else `SERVICE_ACTOR_ID`; omitted when neither). Every strata_rag call sends `X-App-Id: care-bus` and
  `X-Actor-Id` (the calling person, else `SERVICE_ACTOR_ID`). A tenant's own model provider gets none of these.

An upstream that is unset shows as `unconfigured` in the overview; one that is set but unreachable shows as `down` and as an
attention item (its proxy routes answer 503 `upstream_unavailable`).

Callers' side (other services calling this API): `CAREBUS_API_URL` and `CAREBUS_API_TOKEN` (a value from
`CAREBUS_API_SERVICE_TOKENS`), set by the nonagenticai_mcp_server tools and the orchestrator's `HttpProcedures`.

## Sources

`/api/sources` holds customer data sources (website, api, file, datatable) with a field->ontology mapping and
write-only credentials. Responses only say `has_credentials`; no route, fixture, audit row or log returns them. A
secret placed anywhere but the top-level `credentials` object is refused with 422 `secret_value_rejected`.

- `POST /api/sources/{id}/resync` sends the source through strata_ingest into its strata_rag leaf store:
  - website: JSON urls
  - file: the uploaded files, forwarded as multipart (at most 20 files and 50 MB). The forward has its own 30 s timeout, so very large uploads
    to a remote strata_ingest can still time out. A timeout fails the run as "did not confirm the request": strata_ingest
    may have accepted the jobs, so check Ingest jobs before re-syncing.
  - api and datatable: the extended contract. strata_ingest does not accept it yet, so those runs end `unsupported`.
- A run's status is read from strata_ingest when the run or its source is read; nothing polls in the background.
- When a run completes, documents of earlier runs that the new run no longer has are deleted from strata_rag. A
  document that another source's runs still own (same content in the same store) is never deleted. A run that
  completes only partly (some ingest jobs fail) adds its new documents but does not replace the old ones until a
  later run completes in full.
- `DELETE /api/sources/{id}` removes config and history only. `?purge=true` also deletes the source's documents first; if strata_rag is
  unavailable (503 `upstream_unavailable`) or refuses some deletions (409 `purge_incomplete`) the source is kept. A run still
  in progress answers 409.
- `POST /api/sources/{id}/test` (api) makes one guarded GET: public addresses only, pinned DNS, 5 s, 256 KB.
- `POST /api/sources/preview-columns` reads a CSV of at most 10 MB. An `.xlsx` answers 415 (export as CSV); XLSX preview
  waits on an installable `calamine`.
- `schedule` is stored and shown; nothing runs it yet.
- Technical pages' proxies: `/api/ingest/jobs` (list, get, retry, `POST …/cancel`, `DELETE` of an ended job) and
  `/api/knowledge/stores` (create, delete, documents, delete document). Writes are care-admin and audited.
  - Cancel (strata_ingest PR #11): a queued job, or a running one between steps, is `cancelled` at once; a running one
    with a step in flight is `cancelling` until that step ends, then `cancelled`. An ended job is 409
    `ingest_conflict`, except a crawl/sitemap parent that still has open page jobs: those are cancelled.
  - Delete removes an ended job (and a crawl/sitemap parent's page jobs) with its staged files; 409 while it or a child
    is queued/running/cancelling. Its documents stay in strata_rag (`document_ids`, `documents: "retained"`).
- RAG eval: `/api/knowledge/eval/questions` (list, create, PATCH, DELETE, `import` of CSV or JSON) and
  `/api/knowledge/eval/runs` (run, list, get) over strata_rag's `/api/eval` (PR #21). Writes are tenant_admin, audited
  and sent with `X-Actor-Id`; strata_rag's codes (`eval_running`, `question_exists`, `eval_not_ready`, `conflict`,
  `too_many_questions`, `invalid_input`) reach the client. A run is async: `POST` is 202 with the run `running`
  (strata_rag scores it in the background); clients poll `GET …/runs/{id}` until `status` is `completed` or
  `failed` (`error`), reading `progress {done, total}` meanwhile.

## Run

Put the variables in a local `.env` (never committed; `.env*` is gitignored) and export them, then:

```bash
cargo build -p carebus-api
./target/debug/carebus-api
```

## Tests

- Redis on `:6391`: `docker run -d --name cb05-redis -p 6391:6391 redis:8.10.1-alpine redis-server --port 6391`
  (tests isolate themselves with per-test key prefixes and never flush). `TEST_REDIS_URL` (else `REDIS_URL`, as CI sets it) overrides the address.
- Postgres 18: each test binary starts one `postgres:18-alpine` container (`TEST_PG_REGISTRY` names the registry to
  pull it from) and gives every test its own fresh database inside it. Set
  `TEST_DATABASE_URL` to use an existing server instead (a fresh database is created per test; no container is started).
- Run: `export TEST_PG_REGISTRY=docker.io/library; cargo test --all`
