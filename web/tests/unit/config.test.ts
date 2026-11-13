import { describe, expect, it } from 'vitest';
import { missingConfig, readConfig } from '../../src/lib/config';

const full = {
	PUBLIC_CAREBUS_API_URL: 'https://care-bus-api.example.com',
	PUBLIC_KEYCLOAK_URL: 'https://auth.nonagentic.ai',
	PUBLIC_KEYCLOAK_REALM: 'portal',
	PUBLIC_KEYCLOAK_CLIENT_ID: 'care-bus',
};

describe('config', () => {
	it('reads all four values and strips a trailing slash from the API URL', () => {
		const cfg = readConfig({ ...full, PUBLIC_CAREBUS_API_URL: 'https://care-bus-api.example.com/' });
		expect(cfg).toEqual({
			apiUrl: 'https://care-bus-api.example.com',
			keycloakUrl: 'https://auth.nonagentic.ai',
			realm: 'portal',
			clientId: 'care-bus',
		});
	});
	it('lists every missing, blank or "undefined" value by its env name', () => {
		expect(
			missingConfig({
				...full,
				PUBLIC_KEYCLOAK_REALM: '  ',
				PUBLIC_KEYCLOAK_CLIENT_ID: 'undefined',
				PUBLIC_CAREBUS_API_URL: undefined,
			}),
		).toEqual(['PUBLIC_CAREBUS_API_URL', 'PUBLIC_KEYCLOAK_REALM', 'PUBLIC_KEYCLOAK_CLIENT_ID']);
		expect(missingConfig(full)).toEqual([]);
	});
	it('refuses to build a config with missing values', () => {
		expect(() => readConfig({ ...full, PUBLIC_KEYCLOAK_URL: '' })).toThrow(/PUBLIC_KEYCLOAK_URL/);
	});
});
