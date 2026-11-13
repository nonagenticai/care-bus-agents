import { beforeEach, describe, expect, it, vi } from 'vitest';

const cfg = { apiUrl: 'https://api.test', keycloakUrl: 'https://auth.test', realm: 'r', clientId: 'c' };

type Fake = {
	init: ReturnType<typeof vi.fn>;
	updateToken: ReturnType<typeof vi.fn>;
	login: ReturnType<typeof vi.fn>;
	logout: ReturnType<typeof vi.fn>;
	token?: string;
	tokenParsed: unknown;
};
let fake: Fake;

async function freshAuth() {
	vi.resetModules();
	fake = {
		init: vi.fn().mockResolvedValue(true),
		updateToken: vi.fn().mockResolvedValue(true),
		login: vi.fn().mockResolvedValue(undefined),
		logout: vi.fn().mockResolvedValue(undefined),
		token: 'tok-1',
		tokenParsed: { sub: 'u1', name: 'A', preferred_username: 'a.bakker' },
	};
	vi.doMock('keycloak-js', () => ({
		default: function () {
			return fake;
		},
	}));
	const mod = await import('../../src/lib/auth/keycloak');
	await mod.initAuth(cfg);
	return mod;
}

describe('initAuth', () => {
	it('resolves the user from the token and reads no roles from it', async () => {
		const mod = await freshAuth();
		await expect(mod.initAuth(cfg)).resolves.toEqual({ sub: 'u1', name: 'A', actor: 'a.bakker' });
	});
});

describe('getToken', () => {
	beforeEach(() => vi.clearAllMocks());

	it('refreshes within 30 s by default and returns the token', async () => {
		const { getToken } = await freshAuth();
		await expect(getToken()).resolves.toBe('tok-1');
		expect(fake.updateToken).toHaveBeenCalledWith(30);
	});

	it('force refreshes unconditionally with updateToken(-1)', async () => {
		const { getToken } = await freshAuth();
		await getToken({ force: true });
		expect(fake.updateToken).toHaveBeenCalledWith(-1);
	});

	it('a failed refresh sends the user to login and returns null', async () => {
		const { getToken } = await freshAuth();
		fake.updateToken.mockRejectedValueOnce(new Error('refresh token expired'));
		await expect(getToken()).resolves.toBeNull();
		expect(fake.login).toHaveBeenCalledTimes(1);
	});
});

describe('logout', () => {
	it('returns to the app root, never the current URL with its #state fragment (Keycloak 400)', async () => {
		const { logout, logoutRedirect } = await freshAuth();
		window.history.replaceState(null, '', '/admin#state=abc&session_state=def');
		await logout();
		expect(fake.logout).toHaveBeenCalledWith({ redirectUri: `${window.location.origin}/` });
		expect(logoutRedirect('https://cust.nonagentic.ai')).toBe('https://cust.nonagentic.ai/');
	});
});
