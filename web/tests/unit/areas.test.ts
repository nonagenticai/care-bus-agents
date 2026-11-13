import { describe, expect, it } from 'vitest';
import { areaLabel, areasOf } from '../../src/lib/areas';

// Inline inputs (F7): the bootstrap job's four areas, as its context spaces name them.
const boot = [
	{ area: 'faq' },
	{ area: 'logistics' },
	{ area: 'order-support' },
	{ area: 'order-support' },
	{ area: 'payments' },
];

describe('areas from configuration', () => {
	it('labels the bootstrap areas as before', () => {
		expect(areaLabel('order-support')).toBe('Order support');
		expect(areaLabel('logistics')).toBe('Logistics');
		expect(areaLabel('payments')).toBe('Payments');
		expect(areaLabel('faq')).toBe('FAQ');
		expect(areaLabel('returns_desk')).toBe('Returns desk');
	});

	it('areasOf lists each area once, sorted by label', () => {
		expect(areasOf(boot)).toEqual([
			['faq', 'FAQ'],
			['logistics', 'Logistics'],
			['order-support', 'Order support'],
			['payments', 'Payments'],
		]);
	});

	it('a space in a new area adds that area', () => {
		const got = areasOf([...boot, { area: 'returns-desk' }]);
		expect(got).toHaveLength(5);
		expect(got).toContainEqual(['returns-desk', 'Returns desk']);
	});

	it('areasOf keeps an unknown active area', () => {
		expect(areasOf(boot, 'gone')).toContainEqual(['gone', 'Gone']);
		expect(areasOf([], 'faq')).toEqual([['faq', 'FAQ']]);
	});
});
