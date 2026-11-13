// Fails when a build contains the e2e auth shim (see FAKE_AUTH in src/lib/auth/keycloak.ts).
// Run after `npm run build`. The markers are the shim's unique strings.
import { readdirSync, readFileSync, statSync, existsSync } from 'node:fs';
import { join } from 'node:path';

const MARKERS = ['E2E User', 'e2e-token'];
const roots = ['build/client', 'build/server'];

function* walk(dir) {
	for (const name of readdirSync(dir)) {
		const p = join(dir, name);
		if (statSync(p).isDirectory()) yield* walk(p);
		else yield p;
	}
}

if (!existsSync('build/client')) {
	console.error('verify:no-e2e-shim: build/client not found. Run `npm run build` first.');
	process.exit(2);
}

const hits = [];
let files = 0;
for (const root of roots.filter(existsSync)) {
	for (const f of walk(root)) {
		files++;
		const text = readFileSync(f, 'utf8');
		for (const m of MARKERS) if (text.includes(m)) hits.push(`${f}: ${m}`);
	}
}
if (hits.length) {
	console.error(`verify:no-e2e-shim: FAILED, the e2e auth shim is in this build:\n  ${hits.join('\n  ')}`);
	process.exit(1);
}
console.log(`verify:no-e2e-shim: ok (${files} files scanned, no e2e shim markers)`);
