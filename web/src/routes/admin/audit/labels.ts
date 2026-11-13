import type { AuditRow } from '$lib/api/types';

export const ACTOR_TYPES: [string, string][] = [
	['human', 'Person'],
	['ai_agent', 'AI agent'],
	['system', 'System'],
];
export const RESOURCE_TYPES: [string, string][] = [
	['procedure', 'Procedure'],
	['tool', 'Tool'],
	['connector', 'Connector'],
	['model_route', 'Model route'],
	['stream', 'Stream'],
	['dead_letter', 'Dead letter'],
	['ontology_entity', 'Ontology entity'],
	['demo', 'Demo data'],
];
const find = (list: [string, string][], k: string) =>
	list.find(([x]) => x === k)?.[1] ?? k.replaceAll('_', ' ');
export const actorTypeLabel = (t: string) => find(ACTOR_TYPES, t);
export const resourceTypeLabel = (t: string) => find(RESOURCE_TYPES, t);

/** Summary rows for the drawer: every audit_logs column except `details`, which renders as JSON below. */
export function auditSummary(r: AuditRow): [string, string][] {
	return [
		['When', r.timestamp.replace('T', ' ').slice(0, 19) + (r.timestamp.endsWith('Z') ? ' UTC' : '')],
		['Actor', `${actorTypeLabel(r.actor_type)} · ${r.actor_id}`],
		['Action', r.action_type],
		['Resource', `${resourceTypeLabel(r.resource_type)} · ${r.resource_id}`],
		['Status', r.status === 'success' ? 'Success' : 'Failure'],
		['Request id', r.request_id ?? '—'],
		['IP address', r.ip_address ?? '—'],
	];
}
