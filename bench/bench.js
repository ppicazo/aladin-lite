/**
 * Benchmark harness for FITS image layers.
 *
 * Measures the things that actually hurt on large files: wall-clock time to a
 * visible image, bytes pulled off the network, WASM and JS heap growth, and how
 * badly the main thread is blocked while it happens.
 */

import A from '../src/js/A.js';
import init from '../src/core/pkg';

const nowMs = () => performance.now();

/**
 * WASM linear memory, in bytes. Same module instance A.js initialised.
 *
 * WebAssembly memory can grow but never shrink, so the value read after a load
 * is also that load's high-water mark — no sampling needed.
 */
async function wasmHeapBytes() {
    const wasm = await init();
    return wasm.memory.buffer.byteLength;
}

function jsHeapBytes() {
    // Chrome only, and it excludes ArrayBuffers allocated outside the JS heap,
    // so treat it as a lower bound rather than a truth.
    return performance.memory ? performance.memory.usedJSHeapSize : null;
}

/**
 * Records main-thread frame intervals. Any interval much above the display
 * period is time the main thread spent not painting, which for our purposes
 * means it was busy decoding.
 */
class FrameRecorder {
    constructor() {
        this.intervals = [];
        this.running = false;
        this.peakJs = 0;
    }

    start() {
        this.running = true;
        let last = nowMs();
        const tick = () => {
            if (!this.running) return;
            const t = nowMs();
            this.intervals.push(t - last);
            last = t;
            if (performance.memory) {
                this.peakJs = Math.max(this.peakJs, performance.memory.usedJSHeapSize);
            }
            requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
    }

    stop() {
        this.running = false;
    }

    stats() {
        const xs = [...this.intervals].sort((a, b) => a - b);
        if (!xs.length) return {frames: 0};
        const at = (q) => xs[Math.min(xs.length - 1, Math.floor(q * xs.length))];
        return {
            frames: xs.length,
            medianFrameMs: +at(0.5).toFixed(2),
            p95FrameMs: +at(0.95).toFixed(2),
            maxFrameMs: +xs[xs.length - 1].toFixed(2),
            framesOver50ms: xs.filter((d) => d > 50).length,
            framesOver16ms: xs.filter((d) => d > 16.7).length,
        };
    }
}

/** Bytes actually transferred for a given URL, from the Resource Timing API. */
function transferredFor(url) {
    const abs = new URL(url, location.href).href;
    let transferred = 0;
    let encoded = 0;
    let requests = 0;
    for (const e of performance.getEntriesByType('resource')) {
        if (e.name !== abs) continue;
        requests += 1;
        transferred += e.transferSize || 0;
        encoded += e.encodedBodySize || 0;
    }
    return {requests, transferSize: transferred, encodedBodySize: encoded};
}

/** Drive the camera for `durationMs` and report the frame times seen. */
function panZoom(aladin, durationMs) {
    return new Promise((resolve) => {
        const rec = new FrameRecorder();
        rec.start();
        const t0 = nowMs();
        const [ra0, dec0] = aladin.getRaDec();
        const fov0 = aladin.getFov()[0];

        const step = () => {
            const t = (nowMs() - t0) / durationMs;
            if (t >= 1) {
                rec.stop();
                resolve(rec.stats());
                return;
            }
            const a = t * Math.PI * 2;
            aladin.gotoRaDec(ra0 + 0.15 * Math.cos(a), dec0 + 0.15 * Math.sin(a));
            aladin.setFoV(fov0 * (1 - 0.6 * (0.5 - 0.5 * Math.cos(a * 2))));
            requestAnimationFrame(step);
        };
        requestAnimationFrame(step);
    });
}

/**
 * Load `count` copies of `url` as FITS layers and measure the whole thing.
 *
 * Resolves with a plain metrics object; on failure, resolves with `error` set
 * rather than throwing, because a controlled failure at a given file size is
 * itself a result worth recording.
 */
export async function runBench({
    url,
    count = 1,
    container = '#aladin-lite-div',
    panMs = 3000,
    options = {},
    onLog = () => {},
} = {}) {
    const result = {
        url,
        count,
        userAgent: navigator.userAgent,
        crossOriginIsolated: self.crossOriginIsolated === true,
        startedAt: new Date().toISOString(),
    };

    onLog(`init aladin`);
    await A.init;

    const wasmBefore = await wasmHeapBytes();
    const jsBefore = jsHeapBytes();

    const aladin = A.aladin(container, {
        cooFrame: 'icrs',
        log: false,
        backgroundColor: 'rgba(0, 0, 0, 255)',
        showReticle: false,
    });

    const rec = new FrameRecorder();
    rec.start();
    const t0 = nowMs();

    try {
        const loads = [];
        for (let i = 0; i < count; i++) {
            onLog(`displayFITS #${i + 1}/${count}`);
            loads.push(
                new Promise((resolve, reject) => {
                    const tStart = nowMs();
                    aladin.displayFITS(
                        url,
                        {name: `bench-${i}`, colormap: 'viridis', ...options},
                        (ra, dec, fov) => resolve({ra, dec, fov, ms: nowMs() - tStart}),
                        (e) => reject(e instanceof Error ? e : new Error(String(e))),
                        // Distinct layer per image: displayFITS defaults to the
                        // first overlay layer, which would make each call
                        // replace the previous one instead of stacking.
                        `bench-${i}`
                    );
                })
            );
        }

        const loaded = await Promise.all(loads);
        result.loadMs = +(nowMs() - t0).toFixed(1);
        result.perLayerMs = loaded.map((l) => +l.ms.toFixed(1));

        const first = loaded[0];
        aladin.gotoRaDec(first.ra, first.dec);
        aladin.setFoV(first.fov);
    } catch (e) {
        result.loadMs = +(nowMs() - t0).toFixed(1);
        result.error = e && e.message ? e.message : String(e);
    } finally {
        rec.stop();
    }

    result.duringLoad = rec.stats();
    result.peakJsHeapBytes = rec.peakJs || null;
    result.wasmHeapBytes = {
        before: wasmBefore,
        after: await wasmHeapBytes(),
    };
    result.wasmHeapGrowthBytes = result.wasmHeapBytes.after - wasmBefore;
    result.jsHeapBytes = {before: jsBefore, after: jsHeapBytes()};
    result.network = transferredFor(url);

    if (!result.error && panMs > 0) {
        onLog('pan/zoom');
        result.interaction = await panZoom(aladin, panMs);
    }

    result.finishedAt = new Date().toISOString();
    return result;
}

export function formatResult(r) {
    const mib = (b) => (b == null ? 'n/a' : `${(b / (1 << 20)).toFixed(1)} MiB`);
    const lines = [
        `url                 ${r.url}  (x${r.count})`,
        `crossOriginIsolated ${r.crossOriginIsolated}`,
        r.error ? `ERROR               ${r.error}` : `load                ${r.loadMs} ms`,
        `per layer           ${(r.perLayerMs || []).join(', ') || 'n/a'}`,
        `bytes fetched       ${mib(r.network.transferSize)} over ${r.network.requests} request(s)`,
        `wasm heap           ${mib(r.wasmHeapBytes.before)} -> ${mib(r.wasmHeapBytes.after)} (+${mib(r.wasmHeapGrowthBytes)})`,
        `peak js heap        ${mib(r.peakJsHeapBytes)}`,
        `during load         median ${r.duringLoad.medianFrameMs} ms, p95 ${r.duringLoad.p95FrameMs} ms, max ${r.duringLoad.maxFrameMs} ms, ${r.duringLoad.framesOver50ms} frames > 50 ms`,
    ];
    if (r.interaction) {
        lines.push(
            `pan/zoom            median ${r.interaction.medianFrameMs} ms, p95 ${r.interaction.p95FrameMs} ms, max ${r.interaction.maxFrameMs} ms`
        );
    }
    return lines.join('\n');
}
