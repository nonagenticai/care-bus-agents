import { defineConfig, devices } from '@playwright/test';

/**
 * The live, login-gated check. Not part of `npm run test:e2e`: that suite builds a
 * preview with a fake auth shim; this one drives the deployed app and the real
 * Keycloak. Run it on the host, with the fr-test password in the environment
 * (from the gitops SOPS secret); it skips itself otherwise.
 */
export default defineConfig({
	testDir: 'tests/live',
	timeout: 300_000,
	expect: { timeout: 20_000 },
	fullyParallel: false,
	workers: 1,
	retries: 0,
	reporter: [['list'], ['json', { outputFile: '/tmp/carebus-proof-live/report.json' }]],
	outputDir: '/tmp/carebus-proof-live/test-results',
	use: {
		baseURL: 'https://cust.nonagentic.ai',
		ignoreHTTPSErrors: true,
		trace: 'retain-on-failure',
		screenshot: 'only-on-failure',
	},
	projects: [{ name: 'live', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } }],
});
