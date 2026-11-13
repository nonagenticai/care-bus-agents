import { defineConfig, devices } from '@playwright/test';

// Port 3000 is the web app's port everywhere (dev, preview, container); never 8000.
const PORT = 3000;

export default defineConfig({
	testDir: 'tests/e2e',
	use: { baseURL: `http://127.0.0.1:${PORT}` },
	projects: [
		{ name: 'desktop', use: { ...devices['Desktop Chrome'], viewport: { width: 1440, height: 900 } } },
		{ name: 'phone', use: { ...devices['Pixel 7'], viewport: { width: 390, height: 844 } } },
	],
	webServer: {
		command: `npm run build && npx vite preview --host 127.0.0.1 --port ${PORT} --strictPort`,
		port: PORT,
		reuseExistingServer: false,
		timeout: 240_000,
		env: {
			PUBLIC_CAREBUS_API_URL: 'http://api.e2e.test',
			PUBLIC_KEYCLOAK_URL: 'http://auth.e2e.test',
			PUBLIC_KEYCLOAK_REALM: 'portal',
			PUBLIC_KEYCLOAK_CLIENT_ID: 'care-bus',
			// Build-time flag: compiles the auth shim in. Normal builds never set it.
			VITE_E2E_FAKE_AUTH: 'true',
		},
	},
});
