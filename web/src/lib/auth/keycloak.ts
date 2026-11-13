import Keycloak from 'keycloak-js';
import type { AppConfig } from '$lib/config';
import type { User } from './session.svelte';

let kc: Keycloak | null = null;
let initPromise: Promise<User> | null = null;

/**
 * Test-only auth bypass. A BUILD-time flag: only the Playwright webServer build sets VITE_E2E_FAKE_AUTH, so Vite
 * folds this to `false` and drops the shim from every normal build. No runtime env var can switch it on. The shim
 * only signs in; roles come from the (mocked) `GET /api/me` like in production.
 */
const FAKE_AUTH = import.meta.env.VITE_E2E_FAKE_AUTH === 'true';

/**
 * Single-flight: every caller awaits the same init. Resolves the signed-in user; the token's roles are never
 * read (care-bus roles come from `GET /api/me`).
 */
export function initAuth(cfg: AppConfig): Promise<User> {
	if (FAKE_AUTH) return Promise.resolve({ sub: 'e2e', name: 'E2E User', actor: 'e2e' });
	if (initPromise) return initPromise;
	kc = new Keycloak({ url: cfg.keycloakUrl, realm: cfg.realm, clientId: cfg.clientId });
	initPromise = kc
		.init({ onLoad: 'login-required', pkceMethod: 'S256', checkLoginIframe: false })
		.then(() => {
			const t = kc!.tokenParsed as { sub?: string; name?: string; preferred_username?: string };
			return {
				sub: t.sub ?? '',
				name: t.name ?? t.preferred_username ?? '',
				actor: t.preferred_username ?? t.sub ?? '',
			};
		});
	return initPromise;
}

/**
 * Fresh token for every request; refreshes when it expires within 30 s.
 * `force` is for a 401: the server rejected a token keycloak-js still thinks is valid,
 * so `updateToken(30)` would be a no-op. `updateToken(-1)` always refreshes.
 */
export async function getToken(opts: { force?: boolean } = {}): Promise<string | null> {
	if (FAKE_AUTH) return 'e2e-token';
	if (!kc) return null;
	try {
		await kc.updateToken(opts.force ? -1 : 30);
	} catch {
		await kc.login();
		return null;
	}
	return kc.token ?? null;
}

/**
 * Back to the app root after sign-out. Without an explicit `redirectUri` keycloak-js sends the current URL,
 * `#state=…&session_state=…` fragment included, which matches no registered post-logout URI (Keycloak 400).
 */
export function logoutRedirect(origin: string = window.location.origin): string {
	return `${origin}/`;
}

export function logout(): Promise<void> {
	return kc ? kc.logout({ redirectUri: logoutRedirect() }) : Promise.resolve();
}
