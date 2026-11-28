# Care-bus proof harness

Two deployed proof services, one attestation. This is a **manual, one-off** run,
not CI: the bdd job generates a suite with the platform LLM, and the website
scenarios talk to the live chat (two messages in two conversations, each below
the 3-message escalation floor).

## Run

```bash
kubectl apply -k proof/                       # Job + its mounted sources
kubectl -n care-bus logs -f job/carebus-proof # look for ATTESTATION {...}
kubectl -n care-bus delete job/carebus-proof  # the TTL would also reap it
```

`ATTESTATION <json>` (schema `carebus-proof/2`) is the result; the Job exits 0
only when every gating check passes. Delete the Job and its ConfigMap
(`kubectl delete -k proof/`) afterwards.

The login-gated admin case page is a **host** check, not part of the Job: a
super-admin password does not belong in a pod that executes LLM-generated test
code.

```bash
cd web
CAREBUS_FRTEST_PASSWORD="$(sops -d --extract '["stringData"]["password"]' \
  ../../gitops-production/clusters/strato/services/keycloak/secret-frtest.sops.yaml)" \
  npx playwright test -c playwright.live.config.ts
```

## What it proves

- **BDD** (`bdd_playwright_service`, ns `bdd-playwright`): the service GENERATES
  a playwright-bdd package from `https://nonagentic.ai`; this harness downloads
  it, adds `features/website-chat.feature`, runs `npx bddgen && npx playwright
  test`. Two acceptance scenarios are the gating oracle: the widget opens; the
  Care Bus question gets a settled answer within 120 s with no `[n]` marker, no
  literal `\n`, and the product named; an out-of-domain question is refused
  from the sources rather than answered.
- **Generated suite**: its failures are non-gating **only** when each one
  matches an entry in `known-generated-defects.json` (spec title + error
  substring, with the evidence). Any unmatched generated failure fails the Job.
- **Lean** (`lean4-prover`, ns `lean4-prover`): `CareBusModel.lean`, extended
  with one named theorem per vector in `vectors/carebus-vectors.json`, is
  kernel-checked through `POST /api/check`; `NegativeControl.lean` must be
  refused, so the check is non-vacuous.
- **Soundness of that check**: the prover's `valid` is "exit 0 and no error" and
  does not reject `sorry`, so the runner audits `#print axioms` for every
  theorem (refusing `sorryAx` and any axiom outside Lean's three core admits)
  and refuses a `sorry`/`admit` token in the source.
- **Corpus agreement**: the Rust unit test
  `the_proof_vectors_are_what_the_code_does` asserts the same JSON, so the Lean
  model cannot drift from the shipped `without_citation_markers` /
  `compose_reply` without one of the two sides failing.
- **Latency**: each acceptance answer's measured time is recorded and gated at
  45 s.

## What it does not prove

- The Lean model is not a universal refinement of the Rust code: it is checked
  against a shared, Rust-asserted corpus and a length theorem. A Rust change
  outside the corpus is not caught by the model.
- `unescaped_newlines` is out of the Lean model (covered by the Rust tests).
- The admin case page is proven by the live Playwright spec under
  `web/tests/live/` (both viewports), which is host-run, not attested by the
  Job. Telegram and the other channels are not covered.
- One pathological input is stated, not hidden: adjacent brackets can synthesise
  a new marker, so the one-pass stripper is not idempotent on `[[1]2]`. The
  citation shapes the models produce never place `[` directly against `[`.
