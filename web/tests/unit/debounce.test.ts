import { describe, expect, it, vi } from 'vitest';
import { debounce } from '../../src/lib/debounce';

describe('debounce', () => {
	it('calls once with the last arguments after the quiet period', () => {
		vi.useFakeTimers();
		const fn = vi.fn();
		const d = debounce(fn, 250);
		d('a');
		d('ab');
		d('abc');
		vi.advanceTimersByTime(249);
		expect(fn).not.toHaveBeenCalled();
		vi.advanceTimersByTime(1);
		expect(fn).toHaveBeenCalledExactlyOnceWith('abc');
		vi.useRealTimers();
	});
});
