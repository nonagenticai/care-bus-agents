// One-off helper: prints the scoped <style> a design view injects, so it can be pasted into the page.
// Usage: node scripts/extract-view-css.mjs dashboard
import { readFileSync } from 'node:fs';
const view = process.argv[2];
if (!view) {
	console.error('usage: node scripts/extract-view-css.mjs <view>');
	process.exit(2);
}
const src = readFileSync(new URL(`../../design/admin/views/${view}.js`, import.meta.url), 'utf8');
const m = src.match(/insertAdjacentHTML\('beforeend',\s*`<style>([\s\S]*?)<\/style>`\)/);
if (!m) {
	console.error(`no scoped <style> in views/${view}.js`);
	process.exit(1);
}
process.stdout.write(m[1].trim() + '\n');
