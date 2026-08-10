#!/usr/bin/env node
/**
 * Headless benchmark runner.
 *
 * Boots the Vite dev server, drives examples/al-bench-fits.html in Chromium for
 * each case, and writes JSON plus a Markdown table.
 *
 *   node bench/run.mjs                       # default case list
 *   node bench/run.mjs --cases 64mb,512mb    # subset
 *   node bench/run.mjs --headed              # watch it happen
 */

import {spawn} from 'node:child_process';
import {mkdir, writeFile} from 'node:fs/promises';
import {chromium} from '@playwright/test';

const PORT = 5199;
const DATA_PORT = 5200;
const BASE = `http://localhost:${PORT}`;
// Vite answers 500 for multi-gigabyte static files, so benchmark data comes
// from bench/serve.mjs, which streams and speaks Range.
const DATA = `http://localhost:${DATA_PORT}`;

const CASES = {
    '64mb': {file: `${DATA}/synth-64mb.fits`, count: 1, timeoutMs: 120_000},
    '512mb': {file: `${DATA}/synth-512mb.fits`, count: 1, timeoutMs: 300_000},
    // 1 GB and 2 GB bracket the size at which the load-it-all path gives up.
    '1gb': {file: `${DATA}/synth-1gb.fits`, count: 1, timeoutMs: 300_000},
    '2gb': {file: `${DATA}/synth-2gb.fits`, count: 1, timeoutMs: 300_000},
    '4gb': {file: `${DATA}/synth-4gb.fits`, count: 1, timeoutMs: 240_000},
    '64mb-x4': {file: `${DATA}/synth-64mb.fits`, count: 4, timeoutMs: 300_000},
    '64mb-x10': {file: `${DATA}/synth-64mb.fits`, count: 10, timeoutMs: 600_000},
    '512mb-x4': {file: `${DATA}/synth-512mb.fits`, count: 4, timeoutMs: 600_000},
};

// The default DSS2 base layer needs the internet. When it is unreachable the
// console fills with CORS and fetch failures that have nothing to do with what
// is being measured.
const OFFLINE_NOISE = /alaskybis|MocServer|ipac\.caltech|hips|Failed to fetch|net::ERR_FAILED/i;

function arg(name, fallback) {
    const i = process.argv.indexOf(`--${name}`);
    return i === -1 ? fallback : process.argv[i + 1];
}

function startDataServer() {
    const proc = spawn('node', ['bench/serve.mjs', '--port', String(DATA_PORT)], {
        cwd: new URL('..', import.meta.url).pathname,
        stdio: ['ignore', 'inherit', 'inherit'],
    });
    return proc;
}

async function startVite() {
    const proc = spawn('npx', ['vite', '--port', String(PORT), '--strictPort'], {
        cwd: new URL('..', import.meta.url).pathname,
        stdio: ['ignore', 'pipe', 'pipe'],
    });

    await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('vite did not start in 60s')), 60_000);
        const onData = (buf) => {
            if (buf.toString().includes('ready in') || buf.toString().includes(String(PORT))) {
                clearTimeout(timer);
                resolve();
            }
        };
        proc.stdout.on('data', onData);
        proc.stderr.on('data', onData);
        proc.on('exit', (code) => reject(new Error(`vite exited with ${code}`)));
    });

    // Vite prints "ready" slightly before it will answer requests reliably.
    for (let i = 0; i < 40; i++) {
        try {
            const r = await fetch(`${BASE}/examples/al-bench-fits.html`);
            if (r.ok) break;
        } catch {}
        await new Promise((r) => setTimeout(r, 250));
    }

    return proc;
}

function launchBrowser() {
    return chromium.launch({
        headless: arg('headed', null) === null,
        args: [
            // performance.memory is quantised to 100 KB buckets without this.
            '--enable-precise-memory-info',
            // Headless Chromium needs a software GL backend for WebGL2.
            '--use-gl=angle',
            '--use-angle=swiftshader',
            '--enable-unsafe-swiftshader',
        ],
    });
}

async function runCase(browser, name, spec) {
    const context = await browser.newContext();
    const page = await context.newPage();

    const consoleErrors = [];
    const note = (text) => {
        if (!OFFLINE_NOISE.test(text)) consoleErrors.push(text.slice(0, 400));
    };
    page.on('console', (m) => {
        if (m.type() === 'error') note(m.text());
    });
    page.on('pageerror', (e) => note(`pageerror: ${String(e)}`));
    const crashed = new Promise((resolve) => page.on('crash', () => resolve('page crashed (out of memory)')));

    const url = `${BASE}/examples/al-bench-fits.html?file=${encodeURIComponent(spec.file)}&count=${spec.count}&pan=3000`;
    process.stderr.write(`\n=== ${name} ===\n${url}\n`);

    const t0 = Date.now();
    let result;
    try {
        await page.goto(url, {timeout: 60_000});
        const settled = await Promise.race([
            page
                .waitForFunction(() => window.__benchResult !== undefined, null, {timeout: spec.timeoutMs})
                .then(() => 'ok'),
            crashed,
        ]);

        result =
            settled === 'ok'
                ? await page.evaluate(() => window.__benchResult)
                : {error: settled};
    } catch (e) {
        result = {error: `harness: ${String(e).split('\n')[0]}`};
    }

    result.case = name;
    result.wallMs = Date.now() - t0;
    if (consoleErrors.length) result.consoleErrors = consoleErrors.slice(0, 10);

    // A browser that ran out of memory may never answer again, so do not block
    // the rest of the suite waiting for a clean teardown.
    await withDeadline(context.close(), 15_000, 'context close').catch(() => {});
    return result;
}

/**
 * Reject after `ms` unless the promise settles first.
 *
 * Playwright's own timeouts assume the browser is still able to reply. When a
 * case exhausts memory the browser process can die mid-call and leave an
 * awaited promise pending forever, which wedges the whole suite — this is the
 * outer guard for that.
 */
function withDeadline(promise, ms, what) {
    let timer;
    return Promise.race([
        promise.finally(() => clearTimeout(timer)),
        new Promise((_, reject) => {
            timer = setTimeout(() => reject(new Error(`${what} exceeded ${ms} ms`)), ms);
        }),
    ]);
}

function mib(b) {
    return b == null ? 'n/a' : `${(b / (1 << 20)).toFixed(0)} MiB`;
}

function toMarkdown(results) {
    const head =
        '| case | outcome | load | bytes fetched | wasm heap growth | peak JS heap | max frame during load | pan/zoom p95 |\n' +
        '|---|---|---|---|---|---|---|---|';
    const rows = results.map((r) => {
        const outcome = r.error ? `**FAIL** — ${r.error}` : 'ok';
        return [
            r.case,
            outcome,
            r.loadMs != null ? `${(r.loadMs / 1000).toFixed(1)} s` : 'n/a',
            r.network ? mib(r.network.transferSize) : 'n/a',
            mib(r.wasmHeapGrowthBytes),
            mib(r.peakJsHeapBytes),
            r.duringLoad && r.duringLoad.maxFrameMs != null ? `${r.duringLoad.maxFrameMs} ms` : 'n/a',
            r.interaction ? `${r.interaction.p95FrameMs} ms` : 'n/a',
        ].join(' | ');
    });
    return `${head}\n| ${rows.join(' |\n| ')} |`;
}

const wanted = (arg('cases', Object.keys(CASES).join(','))).split(',').filter(Boolean);
const unknown = wanted.filter((c) => !CASES[c]);
if (unknown.length) {
    console.error(`unknown case(s): ${unknown.join(', ')}\nknown: ${Object.keys(CASES).join(', ')}`);
    process.exit(2);
}

const dataServer = startDataServer();
const vite = await startVite();

const results = [];
try {
    for (const name of wanted) {
        const spec = CASES[name];
        // One browser per case. A case large enough to exhaust memory can take
        // the whole browser process down with it, and a shared browser would
        // carry that failure into every case after it.
        const browser = await launchBrowser();
        let r;
        try {
            r = await withDeadline(
                runCase(browser, name, spec),
                spec.timeoutMs + 120_000,
                `case ${name}`
            );
        } catch (e) {
            r = {case: name, error: `harness: ${String(e).split('\n')[0]}`, wallMs: spec.timeoutMs};
        } finally {
            await withDeadline(browser.close(), 15_000, 'browser close').catch(() => {});
        }
        results.push(r);
        process.stderr.write(`${r.error ? 'FAIL' : 'ok'}  ${r.case}  ${(r.wallMs / 1000).toFixed(1)}s\n`);
    }
} finally {
    vite.kill('SIGTERM');
    dataServer.kill('SIGTERM');
}

await mkdir(new URL('results/', import.meta.url), {recursive: true});
const stamp = new Date().toISOString().replace(/[:.]/g, '-');
const jsonPath = new URL(`results/${stamp}.json`, import.meta.url);
await writeFile(jsonPath, JSON.stringify(results, null, 2));

process.stdout.write(`\n${toMarkdown(results)}\n\nwrote ${jsonPath.pathname}\n`);
