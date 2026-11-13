export type AppConfig = { apiUrl: string; keycloakUrl: string; realm: string; clientId: string };
type Env = Record<string, string | undefined>;

const KEYS = [
	'PUBLIC_CAREBUS_API_URL',
	'PUBLIC_KEYCLOAK_URL',
	'PUBLIC_KEYCLOAK_REALM',
	'PUBLIC_KEYCLOAK_CLIENT_ID',
] as const;

export function missingConfig(env: Env): string[] {
	return KEYS.filter((k) => {
		const v = env[k];
		return typeof v !== 'string' || v.trim() === '' || v === 'undefined';
	});
}

export function readConfig(env: Env): AppConfig {
	const missing = missingConfig(env);
	if (missing.length) throw new Error(`Missing runtime config: ${missing.join(', ')}`);
	return {
		apiUrl: env.PUBLIC_CAREBUS_API_URL!.replace(/\/+$/, ''),
		keycloakUrl: env.PUBLIC_KEYCLOAK_URL!,
		realm: env.PUBLIC_KEYCLOAK_REALM!,
		clientId: env.PUBLIC_KEYCLOAK_CLIENT_ID!,
	};
}
