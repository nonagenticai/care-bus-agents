import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
	plugins: [sveltekit()],
	test: {
		environment: 'jsdom',
		include: ['tests/unit/**/*.test.ts'],
		setupFiles: [],
		// The default forks pool cannot terminate its workers on the ARC CI
		// runners: every test passes and vitest then dies with "kill EACCES"
		// (measured 2026-11-10, three identical attempts). The threads pool runs
		// the identical suite without spawning child processes to signal.
		pool: 'threads',
	},
	resolve: process.env.VITEST ? { conditions: ['browser'] } : undefined,
});
