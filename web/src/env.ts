import { defineEnvVars } from '@sveltejs/kit/env';

// Public configuration read at runtime, never baked into the build (the shape agent's web uses).
// Unset values fall back to `''` so `missingConfig` reports them by name instead of the build
// failing — kit 3 replaced `$env/dynamic/public` with this declaration plus named imports from
// `$app/env/public`.
export const variables = defineEnvVars({
	PUBLIC_CAREBUS_API_URL: { public: true, schema: (value) => value ?? '' },
	PUBLIC_CAREBUS_ENVIRONMENT: { public: true, schema: (value) => value ?? '' },
	PUBLIC_CAREBUS_JOB_LABEL: { public: true, schema: (value) => value ?? '' },
	PUBLIC_KEYCLOAK_URL: { public: true, schema: (value) => value ?? '' },
	PUBLIC_KEYCLOAK_REALM: { public: true, schema: (value) => value ?? '' },
	PUBLIC_KEYCLOAK_CLIENT_ID: { public: true, schema: (value) => value ?? '' },
});
