import { describe, expect, it } from 'vitest';
import { schemaFields } from '../../src/routes/admin/jobs/[job]/tools/schema';

describe('tool input schema table', () => {
	it('lists properties with type, required flag and description, required first', () => {
		const rows = schemaFields({
			type: 'object',
			properties: {
				note: { type: 'string', description: 'Free text' },
				order_id: { type: 'string', description: 'Order number' },
				lines: { type: 'array', items: { type: 'integer' } },
				mode: { enum: ['fast', 'slow'] },
			},
			required: ['order_id'],
		});
		expect(rows[0]).toEqual({
			name: 'order_id',
			type: 'string',
			required: true,
			description: 'Order number',
		});
		expect(rows.find((r) => r.name === 'lines')?.type).toBe('integer[]');
		expect(rows.find((r) => r.name === 'mode')).toMatchObject({
			type: 'enum',
			description: 'One of: fast, slow.',
		});
		expect(rows.filter((r) => r.required)).toHaveLength(1);
	});

	it('returns nothing for a schema without properties', () => {
		expect(schemaFields(null)).toEqual([]);
		expect(schemaFields({ type: 'object' })).toEqual([]);
		expect(schemaFields('x')).toEqual([]);
	});
});
