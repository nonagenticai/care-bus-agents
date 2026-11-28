/**
 * The care-bus proof runner.
 *
 * Two independent proof services, one attestation:
 *
 *   1. bdd_playwright_service GENERATES a playwright-bdd package from the live
 *      website (it is a generator, not a runner). This script starts the job,
 *      waits, downloads the package, adds `website-chat.feature` + its steps,
 *      installs the package, and runs `npx bddgen && npx playwright test`.
 *      The generated suite is a smoke signal; the acceptance scenario (ours) is
 *      the gating oracle for the website answer.
 *   2. lean4-prover kernel-checks `CareBusModel.lean` via POST /api/check and
 *      must REFUSE `NegativeControl.lean`. A prover that accepts both proves
 *      nothing.
 *
 * One-shot by design: run it as the Job in k8s/job.yaml. It prints one line
 * `ATTESTATION <json>` and exits non-zero when a gating check fails.
 */

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';

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
  const acceptance = specs.find((spec) => spec.title === 'A visitor asks what Care Bus is');
  let answer = '';
  try {
    answer = (await readFile(path.join(pkg, '.proof-answer.txt'), 'utf8')).trim();
  } catch {
    // The step writes it; absent means the acceptance scenario never reached its Then.
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
      specs: specs.length - (acceptance ? 1 : 0),
      stats: report.stats ?? null,
      failures: specs
        .filter((spec) => spec.ok === false)
        .map((spec) => spec.title)
        .slice(0, 20),
      failure_details: specs
        .filter((spec) => spec.ok === false)
        .map((spec) => ({
          title: spec.title,
          error: String(spec.tests?.[0]?.results?.find((r) => r.error)?.error?.message ?? '').slice(0, 1200),
        }))
        .slice(0, 10),
    },
    acceptance: acceptance
      ? {
          title: acceptance.title,
          ok: acceptance.ok === true,
          annotations: acceptance.tests?.[0]?.annotations ?? [],
        }
      : { title: 'A visitor asks what Care Bus is', ok: false, annotations: [], missing: true },
    answer,
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

  const check = async (file) => {
    const source = await readFile(path.join(PROOF_ROOT, file), 'utf8');
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
      stdout: String(response.stdout ?? '').slice(0, 1200),
      stderr: String(response.stderr ?? '').slice(0, 1200),
      diagnostics: (response.diagnostics ?? []).slice(0, 10),
    };
  };

  return {
    service: 'lean4-prover',
    main: await check('CareBusModel.lean'),
    negative: await check('NegativeControl.lean'),
  };
}

async function main() {
  await mkdir(WORK_DIR, { recursive: true });
  const failures = [];
  const attestation = {
    schema: 'carebus-proof/1',
    started_at: new Date().toISOString(),
  };

  try {
    attestation.lean = await leanProof(WORK_DIR);
    if (!attestation.lean.main.valid) failures.push('lean: CareBusModel.lean was not valid');
    if (attestation.lean.negative.valid) failures.push('lean: NegativeControl.lean was accepted (vacuous check)');
  } catch (error) {
    failures.push(`lean: ${error.message}`);
    attestation.lean = { error: error.message };
  }

  try {
    attestation.bdd = await bddProof(WORK_DIR);
    if (!attestation.bdd.acceptance.ok) failures.push('bdd: the acceptance scenario did not pass');
    if (attestation.bdd.generated.specs < 1) failures.push('bdd: the generated suite carried no other scenario');
    if (!attestation.bdd.answer) failures.push('bdd: no answer text was captured');
    if (/\[\d{1,3}(?:\s*,\s*\d{1,3})*\]/.test(attestation.bdd.answer)) {
      failures.push('bdd: the answer still contains a citation marker');
    }
    if (attestation.bdd.answer.includes('\\n')) failures.push('bdd: the answer contains a literal \\n');
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

main().catch((error) => {
  console.error(`proof runner crashed: ${error.stack ?? error.message}`);
  process.exit(2);
});
