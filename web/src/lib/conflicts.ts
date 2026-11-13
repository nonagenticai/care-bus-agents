import type { RefinementOp, ReleaseConflict } from './api/types';
import { keyTextOf, type SideObject, KEY_COLUMNS } from './refinements';

/** What a release conflict means for the tenant (release/conflicts.rs kinds). */
export function conflictText(c: Pick<ReleaseConflict, 'kind'>): string {
	switch (c.kind) {
		case 'base_changed':
			return 'The release changed this; your refinement still applies.';
		case 'base_removed':
			return 'The release removed this; your refinement has nothing under it.';
		case 'base_added':
			return 'The release now defines this too; yours wins until you choose.';
		case 'parent_missing':
			return 'What this belongs to is gone from the release.';
		case 'base_shadowed':
			return 'Your published version shadows a changed release version.';
	}
}

/**
 * The resolutions of an open conflict: revert ("Accept base", drops the tenant row) always; rebase ("Keep mine",
 * accepts the current base under it) only where a base row still exists. A resolved conflict offers nothing.
 */
export function conflictActions(
	c: Pick<ReleaseConflict, 'kind' | 'resolved_at'>,
	canWrite: boolean,
): RefinementOp[] {
	if (!canWrite || c.resolved_at) return [];
	return c.kind === 'base_removed' || c.kind === 'parent_missing' ? ['revert'] : ['revert', 'rebase'];
}

export const CONFLICT_ACTION_LABEL: Partial<Record<RefinementOp, string>> = {
	revert: 'Accept base',
	rebase: 'Keep mine',
};

/** A conflict's key for people: the natural key values in column order, else the raw key. */
export function conflictKeyText(c: Pick<ReleaseConflict, 'object' | 'key'>): string {
	if (c.object in KEY_COLUMNS) return keyTextOf(c.object as SideObject, c.key);
	return Object.values(c.key).join(' · ');
}

export const OBJECT_LABEL: Record<string, string> = {
	context_space: 'Context space',
	ontology_entity: 'Entity',
	ontology_attribute: 'Attribute',
	ontology_relation: 'Relation',
	procedure: 'Procedure',
	connector: 'Connector',
	connector_field_mapping: 'Field mapping',
	knowledge_store: 'Knowledge store',
	job_setting: 'Job setting',
};
