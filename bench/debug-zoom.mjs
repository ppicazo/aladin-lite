#!/usr/bin/env node
/**
 * Load a streamed FITS, then zoom in and screenshot at each step.
 *
 * The overview is a single 512x512 tile over the whole image, so at high zoom
 * it can only be a blur. What these shots are for is whether refinement fills
 * in real detail as the camera closes in.
 *
 *   node bench/debug-zoom.mjs [url] [prefix]
 */

import {spawn} from 'node:child_process';
import {chromium} from '@playwright/test';

const cwd = new URL('..', import.meta.url).pathname;
const file = process.argv[2] || 'http://localhost:5200/synth-4gb.fits';
const prefix = process.argv[3] || 'bench/zoom';
// How long to let tile reads land before capturing. Full resolution on a very
// wide image needs hundreds of range requests per tile, so it wants longer.
const settleMs = Number(process.argv[4] || 6000);

// An https URL means measuring over HTTP/2, which needs TLS and therefore a
// certificate the browser is told to ignore.
const useHttp2 = file.startsWith('https:');
const certDir = process.env.BENCH_CERT_DIR || '.';

const servers = [
    spawn(
        'node',
        useHttp2
            ? ['bench/serve.mjs', '--port', '5200', '--http2',
               '--cert', `${certDir}/cert.pem`, '--key', `${certDir}/key.pem`]
            : ['bench/serve.mjs', '--port', '5200'],
        {cwd, stdio: 'inherit', detached: true}
    ),
    spawn('node_modules/.bin/vite', ['--port', '5199', '--strictPort'], {cwd, stdio: 'inherit', detached: true}),
];

for (let i = 0; i < 60; i++) {
    try {
        if ((await fetch('http://localhost:5199/examples/al-streamed-fits.html')).ok) break;
    } catch {}
    await new Promise((r) => setTimeout(r, 500));
}

const browser = await chromium.launch({
    args: [
        '--use-gl=angle',
        '--use-angle=swiftshader',
        '--enable-unsafe-swiftshader',
        ...(useHttp2 ? ['--ignore-certificate-errors'] : []),
    ],
    ignoreHTTPSErrors: true,
});
const page = await browser.newPage({viewport: {width: 900, height: 900}});
page.on('console', (m) => {
    const t = m.text();
    if (!/alasky|ipac|MocServer/.test(t)) console.log(`[${m.type()}] ${t.slice(0, 300)}`);
});
page.on('pageerror', (e) => console.log(`[pageerror] ${e}`));

await page.goto(`http://localhost:5199/examples/al-streamed-fits.html?file=${encodeURIComponent(file)}`);
await page.waitForFunction(() => window.__benchResult !== undefined, null, {timeout: 120_000});

const report = await page.evaluate(() => window.__benchResult);
console.log(JSON.stringify(report));

// Fit-to-image, then close in by a factor of four each step.
const fov0 = report.centered_fov.fov;
for (const [i, factor] of [1, 4, 16, 64].entries()) {
    await page.evaluate(
        ([ra, dec, fov]) => {
            const aladin = window.__aladin;
            aladin.gotoRaDec(ra, dec);
            aladin.setFoV(fov);
        },
        [report.centered_fov.ra, report.centered_fov.dec, fov0 / factor]
    );

    // Give the tile reads time to land.
    await page.waitForTimeout(settleMs);
    const out = `${prefix}-${factor}x.png`;
    await page.locator('#aladin-lite-div').screenshot({path: out});
    console.log(`fov ${(fov0 / factor).toFixed(4)} deg -> ${out}`);
}

await browser.close();
for (const s of servers) {
    try {
        process.kill(-s.pid, 'SIGTERM');
    } catch {}
}
process.exit(0);
