import type { Connector, ConnectorInput } from '$lib/api/types';

/** PUT body for a connector: every writable field, none of the overlay or audit columns. */
export function toConnectorInput(c: Connector): ConnectorInput {
	return {
		id: c.id,
		name: c.name,
		system: c.system,
		base_url: c.base_url,
		auth_type: c.auth_type,
		secret_ref: c.secret_ref,
		timeout_ms: c.timeout_ms,
		rate_limit: c.rate_limit,
		mappings: c.mappings,
	};
}
