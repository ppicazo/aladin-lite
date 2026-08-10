#!/usr/bin/env node
/** Load the probe page once with every console line echoed, for debugging. */

import {spawn} from 'node:child_process';
import {chromium} from '@playwright/test';

const cwd = new URL('..', import.meta.url).pathname;
const file = process.argv[2] || 'http://localhost:5200/synth-64mb.fits';

const data = spawn('node', ['bench/serve.mjs', '--port', '5200'], {cwd, stdio: 'inherit'});
const vite = spawn('npx', ['vite', '--port', '5199', '--strictPort'], {cwd, stdio: 'inherit'});

for (let i = 0; i < 60; i++) {
    try {
        if ((await fetch('http://localhost:5199/examples/al-bench-probe.html')).ok) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
}

const browser = await chromium.launch({args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader']});
const page = await browser.newPage();
page.on('console', (m) => console.log(`[${m.type()}] ${m.text()}`));
page.on('pageerror', (e) => console.log(`[pageerror] ${e}`));
page.on('requestfailed', (r) => console.log(`[requestfailed] ${r.url()} ${r.failure()?.errorText}`));

await page.goto(`http://localhost:5199/examples/al-bench-probe.html?file=${encodeURIComponent(file)}`);

try {
    await page.waitForFunction(() => window.__benchResult !== undefined, null, {timeout: 30_000});
    console.log(JSON.stringify(await page.evaluate(() => window.__benchResult), null, 2));
} catch (e) {
    console.log(`TIMED OUT: ${e}`);
    console.log(`status text: ${await page.evaluate(() => document.getElementById('status').textContent)}`);
    console.log(`summary: ${await page.evaluate(() => document.getElementById('summary').textContent)}`);
}

await browser.close();
vite.kill('SIGTERM');
data.kill('SIGTERM');
process.exit(0);
