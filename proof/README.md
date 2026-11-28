# Care-bus proof harness

Two deployed proof services, one attestation. This is a **manual, one-off** run,
not CI: the bdd job generates a suite with the platform LLM, and the website
scenario talks to the live chat (one message, below the 3-message escalation
floor).

## Run

```bash
kubectl apply -k proof/                       # Job + its mounted sources
kubectl -n care-bus logs -f job/carebus-proof # look for ATTESTATION {...}
kubectl -n care-bus delete job/carebus-proof  # the TTL would also reap it
```

`ATTESTATION <json>` is the result; the Job exits 0 only when every gating check
passes. Delete the Job and its ConfigMap (`kubectl delete -k proof/`) afterwards.

## What it proves

- **BDD** (`bdd_playwright_service`, ns `bdd-playwright`): the service GENERATES
  a playwright-bdd package from `https://nonagentic.ai`; this harness downloads
  it, adds `features/website-chat.feature`, runs `npx bddgen && npx playwright
  test`. The generated suite is a smoke signal; the acceptance scenario is the
  gating oracle: the widget opens, one question is sent, a settled assistant
  bubble arrives within 120 s, and its text has no `[n]` marker, no literal
  `\n`, and mentions Care Bus.
- **Lean** (`lean4-prover`, ns `lean4-prover`): `lean/../CareBusModel.lean` is
  kernel-checked through `POST /api/check`; `NegativeControl.lean` must be
  refused. The model mirrors `without_citation_markers` and the `faq` arm of
  `compose_reply` on `List Char`.

## What it does not prove

- The Lean model is not a refinement of the Rust code: it is checked against the
  same vectors the Rust unit tests pin, not extracted from them.
- `unescaped_newlines` is out of the Lean model (covered by the Rust tests).
- The login-gated admin UI is not in this run. Its live check and the mocked
  Playwright suite are elsewhere; this proof covers the public website chat and
  the reply-composition logic.
- One pathological input is stated, not hidden: adjacent brackets can synthesise
  a new marker, so the one-pass stripper is not idempotent on `[[1]2]`. The
  citation shapes the models produce never place `[` directly against `[`.
