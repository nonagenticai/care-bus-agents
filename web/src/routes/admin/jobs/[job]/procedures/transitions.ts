import type { VersionStatus } from '$lib/api/types';

// Mirrors plan 05 procedure_management allowed(from, to).
const NEXT: Record<VersionStatus, VersionStatus[]> = {
	DRAFT: ['CANARY', 'DISCARDED'],
	CANARY: ['PUBLISHED', 'DISCARDED'],
	PUBLISHED: ['ARCHIVED'],
	// Roll-back: an archived version was PUBLISHED before and can be re-published (archives the current one).
	ARCHIVED: ['PUBLISHED'],
	DISCARDED: [],
};
export const nextStatuses = (s: VersionStatus): VersionStatus[] => NEXT[s];
