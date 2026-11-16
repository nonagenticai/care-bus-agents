import adapter from '@sveltejs/adapter-node';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

export default defineConfig({
	plugins: [
		// SvelteKit configuration lives here since kit 3 — `svelte.config.js` is no
		// longer supported (https://svelte.dev/e/kit/config_file_unsupported).
		sveltekit({
			// Kit 3 removed the built-in `$lib` alias (https://svelte.dev/e/kit/module_removed_lib);
			// aliasing it here is kit's own compatibility path, avoiding a repo-wide
			// rewrite of 76 files to `#lib` subpath imports.
			alias: { $lib: 'src/lib' },
			preprocess: vitePreprocess(),
			adapter: adapter({ out: 'build' }),
		}),
	],
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
	// Kit 3's vite plugin now defaults SSR sourcemaps ON (`config.build?.sourcemap ?? true`),
	// so every server chunk ships a `.js.map` carrying `sourcesContent`. That put the FAKE_AUTH
	// e2e shim's markers back into `build/` and failed `npm run verify:no-e2e-shim`. Kit 2 shipped
	// no maps; keeping the old behaviour also keeps that gate meaningful.
	build: { sourcemap: false },
});
