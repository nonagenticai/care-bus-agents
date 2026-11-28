/**
 * The care-bus proof runner.
 *
 * Two independent proof services, one attestation:
 *
 *   1. bdd_playwright_service GENERATES a playwright-bdd package from the live
 *      website (it is a generator, not a runner). This script starts the job,
 *      waits, downloads the package, adds `website-chat.feature` + its steps,
 *      installs the package, and runs `npx bddgen && npx playwright test`.
 *      Two acceptance scenarios are the gating oracle. A generated failure is
 *      non-gating ONLY when it matches `known-generated-defects.json`; an
 *      unmatched generated failure fails the run.
 *   2. lean4-prover kernel-checks `CareBusModel.lean` via POST /api/check and
 *      must REFUSE `NegativeControl.lean`. The model is assembled with named
 *      theorems from `vectors/carebus-vectors.json` (the same corpus the Rust
 *      test asserts), and the runner audits `#print axioms` for `sorryAx`: the
 *      prover's `valid` alone does not reject `sorry`.
 *
 * One-shot by design: run it as the Job in k8s/job.yaml. It prints one line
 * `ATTESTATION <json>` and exits non-zero when a gating check fails.
 */

import { realpathSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const env = (name, fallback) => {
  const value = process.env[name];
  return value && value.trim() ? value.trim() : fallback;
};

const BDD_URL = env('BDD_URL', 'http://bdd-playwright-service.bdd-playwright.svc.cluster.local:6313');
const BDD_TENANT = env('BDD_TENANT', '01a1ff14-338d-7293-93ea-28e5e17ac4b0');
const BDD_TARGET = env('BDD_TARGET', 'https://nonagentic.ai');
const BDD_JOB_ID = env('BDD_JOB_ID', '');
const BDD_MAX_WAIT_MS = Number(env('BDD_MAX_WAIT_MS', String(45 * 60 * 1000)));
const LEAN_URL = env('LEAN_URL', 'http://lean4-prover.lean4-prover.svc.cluster.local:8812');
const KEYCLOAK_TOKEN_URL = env(
  'KEYCLOAK_TOKEN_URL',
  'http://keycloak.keycloak.svc.cluster.local:8080/realms/secure-apps/protocol/openid-connect/token',
);
const LICENSES_URL = env('LICENSES_URL', 'http://licenses.licenses.svc.cluster.local:8544');
const CLIENT_ID = env('CLIENT_ID', 'care-bus-bundle-minter');
const CLIENT_SECRET_FILE = env('CLIENT_SECRET_FILE', '/secrets/client-secret');
const MINT_TENANT = env('MINT_TENANT', '0195f48b-d000-7000-8000-000000000099');
const MINT_APP = env('MINT_APP', 'strata_rag');
const MINT_BUNDLE = env('MINT_BUNDLE', 'care-bus');
const PROOF_ROOT = env('PROOF_ROOT', '/proof/src');
const WORK_DIR = env('WORK_DIR', '/work');
const VECTORS_FILE = env('VECTORS_FILE', 'carebus-vectors.json');
const KNOWN_DEFECTS_FILE = env('KNOWN_DEFECTS_FILE', 'known-generated-defects.json');
const LATENCY_BUDGET_MS = Number(env('LATENCY_BUDGET_MS', String(45_000)));

// The gating oracle. Every title must be present in the Playwright report and pass.
const ACCEPTANCE_TITLES = [
  'A visitor asks what Care Bus is',
  'A visitor asks something the sources do not cover',
];

// The `#print axioms` lines the model carries. A rename here fails the check.
const MODEL_THEOREMS = [
  'CareBus.trimLeft_length_le',
  'CareBus.trimRight_length_le',
  'CareBus.trim_length_le',
  'CareBus.dropTrailingSpace_length_le',
  'CareBus.takeUntilClose_snd_length_le',
  'CareBus.stripGo_length_le',
  'CareBus.strip_length_le',
];

// Lean's core admits these; anything else (notably `sorryAx`) is refused.
const ALLOWED_AXIOMS = new Set(['propext', 'Classical.choice', 'Quot.sound']);

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function fetchJson(url, { method = 'GET', headers = {}, body, timeoutMs = 60_000 } = {}) {
  const response = await fetch(url, {
    method,
    headers,
    body,
    signal: AbortSignal.timeout(timeoutMs),
  });
  const text = await response.text();
  let json;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  if (!response.ok) {
    throw new Error(`${method} ${url} -> ${response.status}: ${text.slice(0, 800)}`);
  }
  return json;
}

const sha256 = (buffer) => createHash('sha256').update(buffer).digest('hex');

async function bddProof(work) {
  const started = Date.now();
  let jobId = BDD_JOB_ID;
  if (!jobId) {
    const request = {
      url: BDD_TARGET,
      target_pages: ['/'],
      javascript_enabled: true,
      include_negative_tests: false,
      max_scenarios_per_feature: 8,
    };
    const created = await fetchJson(`${BDD_URL}/api/bdd/qa-suite`, {
      method: 'POST',
      headers: { 'X-Tenant-Id': BDD_TENANT, 'content-type': 'application/json' },
      body: JSON.stringify(request),
      timeoutMs: 90_000,
    });
    if (!created?.job_id) throw new Error(`bdd did not return a job_id: ${JSON.stringify(created)}`);
    jobId = created.job_id;
    console.log(`[bdd] job ${jobId} started for ${BDD_TARGET}`);
  } else {
    console.log(`[bdd] reusing job ${jobId}`);
  }

  let status = { job_id: jobId, status: 'pending' };
  while (Date.now() - started < BDD_MAX_WAIT_MS) {
    await sleep(5_000);
    status = await fetchJson(`${BDD_URL}/api/bdd/${jobId}`, {
      headers: { 'X-Tenant-Id': BDD_TENANT },
      timeoutMs: 30_000,
    });
    if (status.status === 'completed' || status.status === 'failed') break;
  }
  if (status.status !== 'completed') {
    throw new Error(
      `bdd job ${jobId} ended ${status.status} after ${Math.round((Date.now() - started) / 1000)}s: ${status.error ?? ''}`,
    );
  }

  const result = await fetchJson(`${BDD_URL}/api/bdd/${jobId}/result`, {
    headers: { 'X-Tenant-Id': BDD_TENANT },
    timeoutMs: 60_000,
  });

  const zipPath = path.join(work, 'package.zip');
  const download = await fetch(`${BDD_URL}/api/bdd/${jobId}/download`, {
    headers: { 'X-Tenant-Id': BDD_TENANT },
    signal: AbortSignal.timeout(120_000),
  });
  if (!download.ok) throw new Error(`bdd download -> ${download.status}`);
  const zip = Buffer.from(await download.arrayBuffer());
  await writeFile(zipPath, zip);
  console.log(`[bdd] package ${zip.length} bytes sha256=${sha256(zip)}`);

  const pkg = path.join(work, 'package');
  await rm(pkg, { recursive: true, force: true });
  await mkdir(pkg, { recursive: true });
  await extractZip(zipPath, pkg);

  // The acceptance feature and its steps join the generated project. The
  // generated config uses `missingSteps: fail-on-gen`, so a missing step fails
  // the run rather than silently stubbing the scenario.
  await mkdir(path.join(pkg, 'features'), { recursive: true });
  await mkdir(path.join(pkg, 'steps'), { recursive: true });
  // readFile/writeFile, NOT cp: the k8s ConfigMap mount exposes each key as a
  // symlink, and a copied symlink makes Node resolve `playwright-bdd` from
  // /proof/src instead of the package's node_modules.
  await writeFile(
    path.join(pkg, 'features/website-chat.feature'),
    await readFile(path.join(PROOF_ROOT, 'website-chat.feature')),
  );
  await writeFile(
    path.join(pkg, 'steps/website-chat.steps.ts'),
    await readFile(path.join(PROOF_ROOT, 'website-chat.steps.ts')),
  );

  // The Job runs as a non-root user under the restricted Pod Security profile,
  // where Chromium's setuid sandbox cannot start. This is a container fact, not
  // a change to what the suite asserts: disable the browser sandbox only.
  const configPath = path.join(pkg, 'playwright.config.ts');
  const config = await readFile(configPath, 'utf8');
  if (!config.includes('--no-sandbox')) {
    await writeFile(
      configPath,
      config.replace('use: {', "use: {\n    launchOptions: { args: ['--no-sandbox'] },"),
    );
  }

  run('npm', ['install', '--no-audit', '--no-fund'], { cwd: pkg, timeoutMs: 10 * 60_000 });
  // The package pins its own @playwright/test; install THAT version's Chromium
  // into the work volume rather than trusting the image's browser build to match.
  const browsers = path.join(work, 'browsers');
  run('npx', ['playwright', 'install', 'chromium'], {
    cwd: pkg,
    timeoutMs: 10 * 60_000,
    env: { PLAYWRIGHT_BROWSERS_PATH: browsers },
  });
  run('npx', ['bddgen'], { cwd: pkg, timeoutMs: 5 * 60_000 });

  const testRun = run('npx', ['playwright', 'test', '--reporter=json'], {
    cwd: pkg,
    timeoutMs: 20 * 60_000,
    allowFailure: true,
    env: {
      ...process.env,
      PW_TEST_TIMEOUT_MS: env('PW_TEST_TIMEOUT_MS', '180000'),
      PLAYWRIGHT_BROWSERS_PATH: browsers,
    },
  });
  const report = parsePlaywrightJson(testRun.stdout);
  const specs = collectSpecs(report.suites);
  if (testRun.code !== 0) {
    console.error(`[bdd] playwright exited ${testRun.code}`);
    console.error(testRun.stderr.slice(-3000));
  }
  const acceptance = ACCEPTANCE_TITLES.map((title) => {
    const spec = specs.find((s) => s.title === title);
    return spec
      ? { title, ok: spec.ok === true, annotations: spec.tests?.[0]?.annotations ?? [] }
      : { title, ok: false, annotations: [], missing: true };
  });
  const answers = await readAnswers(pkg);
  const known = await loadKnownDefects();
  const failing = specs.filter((spec) => spec.ok === false);
  const generatedFailures = failing.filter((spec) => !ACCEPTANCE_TITLES.includes(spec.title));
  const matched = [];
  const unclassified = [];
  for (const spec of generatedFailures) {
    const error = stripAnsi(failureMessage(spec)).slice(0, 1200);
    const entry = known.find((k) => k.spec === spec.title && error.includes(k.error_includes));
    if (entry) matched.push({ spec: spec.title, reason: entry.reason });
    else unclassified.push({ spec: spec.title, error: error.slice(0, 800) });
  }

  return {
    service: 'bdd_playwright_service',
    job_id: jobId,
    target: BDD_TARGET,
    status: status.status,
    features_generated: result.features_generated ?? status.features_generated ?? 0,
    step_defs_generated: result.step_defs_generated ?? status.step_defs_generated ?? 0,
    package_sha256: sha256(zip),
    generated: {
      specs: specs.length - acceptance.filter((a) => !a.missing).length,
      stats: report.stats ?? null,
      failures: failing.map((spec) => spec.title).slice(0, 20),
      failure_details: failing
        .map((spec) => ({ title: spec.title, error: stripAnsi(failureMessage(spec)).slice(0, 1200) }))
        .slice(0, 10),
      known_defects_matched: matched,
      unclassified_failures: unclassified,
      verdict:
        generatedFailures.length === 0
          ? 'green'
          : unclassified.length === 0
            ? 'classified'
            : 'unclassified',
    },
    acceptance,
    answers,
    exit_code: testRun.code,
  };
}

function run(command, args, { cwd, timeoutMs, allowFailure = false, env: extraEnv } = {}) {
  try {
    const stdout = execFileSync(command, args, {
      cwd,
      timeout: timeoutMs,
      maxBuffer: 64 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, ...extraEnv, CI: '1' },
    });
    return { code: 0, stdout: stdout.toString(), stderr: '' };
  } catch (error) {
    if (!allowFailure) {
      throw new Error(
        `${command} ${args.join(' ')} failed (${error.status ?? '?'}): ${String(error.stderr ?? error.message).slice(0, 1500)}`,
      );
    }
    return {
      code: error.status ?? 1,
      stdout: String(error.stdout ?? ''),
      stderr: String(error.stderr ?? ''),
    };
  }
}

function parsePlaywrightJson(stdout) {
  // The reporter writes one JSON object; tolerate npm/npx noise around it.
  const start = stdout.indexOf('{');
  const end = stdout.lastIndexOf('}');
  if (start === -1 || end <= start) throw new Error('playwright produced no JSON report');
  return JSON.parse(stdout.slice(start, end + 1));
}

function collectSpecs(suites, out = []) {
  for (const suite of suites ?? []) {
    for (const spec of suite.specs ?? []) out.push(spec);
    collectSpecs(suite.suites, out);
  }
  return out;
}

// Playwright colours its error messages; the known-defect match is on the plain text.
const stripAnsi = (text) => text.replace(/\u001b\[[0-9;]*[A-Za-z]/g, '');

function failureMessage(spec) {
  return String(spec.tests?.[0]?.results?.find((r) => r.error)?.error?.message ?? '');
}

async function loadKnownDefects() {
  // Fail closed: a missing or unreadable file means NO generated failure is allowed.
  try {
    const doc = JSON.parse(await readFile(path.join(PROOF_ROOT, KNOWN_DEFECTS_FILE), 'utf8'));
    return Array.isArray(doc.generated) ? doc.generated : [];
  } catch {
    return [];
  }
}

async function readAnswers(pkg) {
  const dir = path.join(pkg, '.proof-answers');
  let names = [];
  try {
    names = await readdir(dir);
  } catch {
    return [];
  }
  const answers = [];
  for (const name of names.filter((n) => n.endsWith('.json')).sort()) {
    try {
      answers.push(JSON.parse(await readFile(path.join(dir, name), 'utf8')));
    } catch {
      // A torn artifact is absence, not evidence; the missing-answer gate catches it.
    }
  }
  return answers;
}

async function extractZip(zipPath, destination) {
  try {
    run('unzip', ['-q', '-o', zipPath, '-d', destination], { timeoutMs: 120_000 });
    return;
  } catch {
    // The Playwright image does not guarantee unzip; npm does.
  }
  const harness = '/tmp/carebus-proof-harness';
  await mkdir(harness, { recursive: true });
  run('npm', ['install', '--prefix', harness, '--no-audit', '--no-fund', 'extract-zip@2.0.1'], {
    timeoutMs: 5 * 60_000,
  });
  const require = createRequire(`${harness}/`);
  const extract = require('extract-zip');
  await extract(zipPath, { dir: destination });
}

/** A Lean `List Char` literal. ASCII keeps its readable form; `Char.ofNat` carries the rest. */
function leanChars(text) {
  const parts = [];
  for (const ch of text) {
    const cp = ch.codePointAt(0);
    if (cp === 39) parts.push("'\\''");
    else if (cp === 92) parts.push("'\\\\'");
    else if (cp >= 32 && cp < 127) parts.push(`'${ch}'`);
    else parts.push(`Char.ofNat ${cp}`);
  }
  return `[${parts.join(',')}]`;
}

/**
 * The model source plus one named theorem per vector, then `#print axioms` for every theorem the
 * audit expects. The Rust test reads the same JSON, so a change on either side fails one of them.
 */
function assembleLean(model, vectors) {
  const lines = [
    model.trimEnd(),
    '',
    '/-! Generated from proof/vectors/carebus-vectors.json — the corpus the Rust test asserts. -/',
    // `decide` on the longest live answer exceeds Lean's default recursion depth (measured:
    // faq_vec on the ~130-char answer failed at the default and left a sorryAx).
    'set_option maxRecDepth 400000',
  ];
  const axiomNames = [];
  for (const [i, v] of (vectors.strip ?? []).entries()) {
    const name = `strip_vec_${i}`;
    lines.push(
      `theorem ${name} : CareBus.strip ${v.max} ${leanChars(v.text)} = ${leanChars(v.expected)} := by decide`,
    );
    axiomNames.push(name);
  }
  for (const [i, v] of (vectors.faq ?? []).entries()) {
    const name = `faq_vec_${i}`;
    const numbers = (v.sources ?? []).map((s) => (typeof s.number === 'number' ? `some ${s.number}` : 'none'));
    const citations = (v.citations ?? []).map((_, j) => String(j + 1));
    // `some x` must be parenthesised: Lean application is left-associative, so
    // `composeFaq [] [] some xs` would apply the result of `composeFaq [] []` to `some`.
    const answer = typeof v.answer === 'string' ? `(some ${leanChars(v.answer)})` : 'none';
    const rhs =
      v.kind === 'fallback' ? 'CareBus.Reply.fallback' : `CareBus.Reply.answer ${leanChars(v.expected)}`;
    lines.push(
      `theorem ${name} : CareBus.composeFaq [${numbers.join(', ')}] [${citations.join(', ')}] ${answer} = ${rhs} := by decide`,
    );
    axiomNames.push(name);
  }
  for (const name of axiomNames) lines.push(`#print axioms ${name}`);
  return { source: lines.join('\n'), axiomNames };
}

/** Every expected theorem must print "does not depend on any axioms" or only core admits. */
function axiomReport(stdout, expected) {
  const lines = String(stdout ?? '').split('\n');
  const missing = [];
  const refused = [];
  for (const name of expected) {
    const line = lines.find((l) => l.includes(`'${name}'`));
    if (!line) {
      missing.push(name);
      continue;
    }
    if (line.includes('does not depend on any axioms')) continue;
    const listed = line.match(/depends on axioms: \[(.*)\]/);
    if (!listed) {
      refused.push(`${name}: unparseable axiom line: ${line.trim()}`);
      continue;
    }
    const unexpected = listed[1]
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean)
      .filter((a) => !ALLOWED_AXIOMS.has(a));
    if (unexpected.length) refused.push(`${name}: ${unexpected.join(', ')}`);
  }
  return { clean: missing.length === 0 && refused.length === 0, missing, refused };
}

async function leanProof(work) {
  const secret = (await readFile(CLIENT_SECRET_FILE, 'utf8')).trim();
  if (!secret) throw new Error(`empty client secret in ${CLIENT_SECRET_FILE}`);

  const form = new URLSearchParams({
    grant_type: 'client_credentials',
    client_id: CLIENT_ID,
    client_secret: secret,
  });
  const token = await fetchJson(KEYCLOAK_TOKEN_URL, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body: form.toString(),
    timeoutMs: 30_000,
  });
  if (!token?.access_token) throw new Error('keycloak returned no access_token');

  const mintUrl =
    `${LICENSES_URL}/internal/tenants/${MINT_TENANT}/bundle-token` +
    `?app=${encodeURIComponent(MINT_APP)}&bundle=${encodeURIComponent(MINT_BUNDLE)}&ttl_minutes=60`;
  const mint = await fetchJson(mintUrl, {
    method: 'POST',
    headers: { authorization: `Bearer ${token.access_token}` },
    timeoutMs: 30_000,
  });
  if (!mint?.token) throw new Error('licenses returned no bundle token');

  const check = async (file, source) => {
    const started = Date.now();
    const response = await fetchJson(`${LEAN_URL}/api/check`, {
      method: 'POST',
      headers: { authorization: `Bearer ${mint.token}`, 'content-type': 'application/json' },
      body: JSON.stringify({ source, timeout: 120 }),
      timeoutMs: 180_000,
    });
    return {
      file,
      sha256: sha256(source),
      valid: response.valid === true,
      computation_time_ms: response.computation_time_ms ?? null,
      duration_ms: Date.now() - started,
      // `#print axioms` writes one line per theorem; 1200 chars was the old cap and would truncate.
      stdout: String(response.stdout ?? '').slice(0, 8000),
      stderr: String(response.stderr ?? '').slice(0, 1200),
      diagnostics: (response.diagnostics ?? []).slice(0, 10),
    };
  };

  const rawVectors = await readFile(path.join(PROOF_ROOT, VECTORS_FILE), 'utf8');
  const vectors = JSON.parse(rawVectors);
  const model = await readFile(path.join(PROOF_ROOT, 'CareBusModel.lean'), 'utf8');
  const assembled = assembleLean(model, vectors);
  const main = await check('CareBusModel.lean', assembled.source);
  main.vectors = {
    file: VECTORS_FILE,
    sha256: sha256(rawVectors),
    strip: vectors.strip?.length ?? 0,
    faq: vectors.faq?.length ?? 0,
  };
  main.axioms = axiomReport(main.stdout, MODEL_THEOREMS.concat(assembled.axiomNames));
  // The prover's `valid` treats `sorry` as a warning; the assembled source must not contain one.
  main.sorry_free = !/\b(sorry|admit)\b/.test(assembled.source);
  const negativeSource = await readFile(path.join(PROOF_ROOT, 'NegativeControl.lean'), 'utf8');

  return {
    service: 'lean4-prover',
    main,
    negative: await check('NegativeControl.lean', negativeSource),
  };
}

async function main() {
  await mkdir(WORK_DIR, { recursive: true });
  const failures = [];
  const attestation = {
    schema: 'carebus-proof/2',
    started_at: new Date().toISOString(),
  };

  try {
    attestation.lean = await leanProof(WORK_DIR);
    if (!attestation.lean.main.valid) failures.push('lean: CareBusModel.lean was not valid');
    if (!attestation.lean.main.sorry_free) {
      failures.push('lean: the assembled model contains a sorry/admit token');
    }
    if (!attestation.lean.main.axioms?.clean) {
      const audit = attestation.lean.main.axioms ?? {};
      failures.push(
        `lean: axiom audit failed (missing: ${audit.missing?.join(', ') || '—'}; refused: ${
          audit.refused?.join('; ') || '—'
        })`,
      );
    }
    if (attestation.lean.negative.valid) failures.push('lean: NegativeControl.lean was accepted (vacuous check)');
  } catch (error) {
    failures.push(`lean: ${error.message}`);
    attestation.lean = { error: error.message };
  }

  try {
    attestation.bdd = await bddProof(WORK_DIR);
    for (const scenario of attestation.bdd.acceptance) {
      if (!scenario.ok) failures.push(`bdd: acceptance scenario "${scenario.title}" did not pass`);
    }
    if (attestation.bdd.generated.specs < 1) failures.push('bdd: the generated suite carried no other scenario');
    if (attestation.bdd.generated.unclassified_failures.length) {
      const titles = attestation.bdd.generated.unclassified_failures.map((f) => f.spec).join('; ');
      failures.push(
        `bdd: ${attestation.bdd.generated.unclassified_failures.length} generated failure(s) not in ${KNOWN_DEFECTS_FILE}: ${titles}`,
      );
    }
    for (const title of ACCEPTANCE_TITLES) {
      const answer = attestation.bdd.answers.find(
        (a) => a.title === title || (Array.isArray(a.title_path) && a.title_path.includes(title)),
      );
      if (!answer?.answer) {
        failures.push(`bdd: no answer captured for "${title}"`);
        continue;
      }
      if (/\[\d{1,3}(?:\s*,\s*\d{1,3})*\]/.test(answer.answer)) {
        failures.push(`bdd: "${title}" still contains a citation marker`);
      }
      if (answer.answer.includes('\\n')) failures.push(`bdd: "${title}" contains a literal \\n`);
      if (typeof answer.elapsed_ms === 'number' && answer.elapsed_ms > LATENCY_BUDGET_MS) {
        failures.push(
          `bdd: "${title}" took ${answer.elapsed_ms} ms (budget ${LATENCY_BUDGET_MS} ms)`,
        );
      }
    }
  } catch (error) {
    failures.push(`bdd: ${error.message}`);
    attestation.bdd = { error: error.message };
  }

  attestation.finished_at = new Date().toISOString();
  attestation.failures = failures;
  attestation.verdict = failures.length === 0 ? 'PASS' : 'FAIL';
  console.log(`ATTESTATION ${JSON.stringify(attestation)}`);
  process.exit(failures.length === 0 ? 0 : 1);
}

// Importing this file (for the assembly helpers) must not start a proof run.
// Compare real paths: a Kubernetes ConfigMap volume is a directory of symlinks, so `argv[1]`
// (/proof/src/run.mjs) and Node's resolved `import.meta.url` (.../..data/run.mjs) differ as
// strings while naming the same file. A plain string compare made the Job exit 0 in 3 s with
// no output — a vacuous pass, caught 2026-11-28.
const invokedDirectly =
  Boolean(process.argv[1]) &&
  realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
if (invokedDirectly) {
  main().catch((error) => {
    console.error(`proof runner crashed: ${error.stack ?? error.message}`);
    process.exit(2);
  });
}

export { assembleLean, axiomReport, leanChars };
