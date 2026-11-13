import { describe, expect, it } from 'vitest';
import { nextStatuses } from '../../src/routes/admin/jobs/[job]/procedures/transitions';

describe('procedure status transitions', () => {
	it('offers only lifecycle moves', () => {
		expect(nextStatuses('DRAFT')).toEqual(['CANARY', 'DISCARDED']);
		expect(nextStatuses('CANARY')).toEqual(['PUBLISHED', 'DISCARDED']);
		expect(nextStatuses('PUBLISHED')).toEqual(['ARCHIVED']);
		expect(nextStatuses('ARCHIVED')).toEqual(['PUBLISHED']);
		expect(nextStatuses('DISCARDED')).toEqual([]);
	});
});
