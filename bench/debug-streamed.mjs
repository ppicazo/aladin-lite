#!/usr/bin/env node
/**
 * Load the streamed-FITS page once, echo every console line, and screenshot it.
 *
 *   node bench/debug-streamed.mjs [url] [out.png]
 */

import {spawn} from 'node:child_process';
import {chromium} from '@playwright/test';

const cwd = new URL('..', import.meta.url).pathname;
const file = process.argv[2] || 'http://localhost:5200/synth-4gb.fits';
const out = process.argv[3] || 'bench/streamed.png';

const servers = [
    spawn('node', ['bench/serve.mjs', '--port', '5200'], {cwd, stdio: 'inherit', detached: true}),
    spawn('node_modules/.bin/vite', ['--port', '5199', '--strictPort'], {cwd, stdio: 'inherit', detached: true}),
];

for (let i = 0; i < 60; i++) {
    try {
        if ((await fetch('http://localhost:5199/examples/al-streamed-fits.html')).ok) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
}

const browser = await chromium.launch({
    args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
});
const page = await browser.newPage({viewport: {width: 900, height: 900}});
page.on('console', (m) => console.log(`[${m.type()}] ${m.text().slice(0, 500)}`));
page.on('pageerror', (e) => console.log(`[pageerror] ${e}`));
page.on('requestfailed', (r) => console.log(`[requestfailed] ${r.url()} ${r.failure()?.errorText}`));

await page.goto(`http://localhost:5199/examples/al-streamed-fits.html?file=${encodeURIComponent(file)}`);

try {
    await page.waitForFunction(() => window.__benchResult !== undefined, null, {timeout: 120_000});
    console.log(JSON.stringify(await page.evaluate(() => window.__benchResult), null, 2));
} catch (e) {
    console.log(`TIMED OUT: ${String(e).split('\n')[0]}`);
    console.log(`status: ${await page.evaluate(() => document.getElementById('status').textContent)}`);
}

// Let the renderer settle before capturing.
await page.waitForTimeout(2000);
await page.locator('#aladin-lite-div').screenshot({path: out});
console.log(`screenshot: ${out}`);

await browser.close();
for (const s of servers) {
    try {
        process.kill(-s.pid, 'SIGTERM');
    } catch {}
}
process.exit(0);
