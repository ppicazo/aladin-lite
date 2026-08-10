#!/usr/bin/env node
/**
 * Static file server for benchmark data.
 *
 * Vite's dev server cannot serve multi-gigabyte files (it answers 500), and the
 * streaming FITS work needs byte-range reads anyway, so benchmark data is
 * served from here instead:
 *
 *   - HTTP Range requests, including multi-gigabyte offsets
 *   - permissive CORS, with Accept-Ranges and Content-Range exposed so that
 *     cross-origin range reads work from the app origin
 *   - streamed from disk, so file size does not bound memory
 *
 *   node bench/serve.mjs [--port 5200] [--dir bench/data]
 */

import {createReadStream} from 'node:fs';
import {stat} from 'node:fs/promises';
import {createServer} from 'node:http';
import {extname, join, normalize, resolve} from 'node:path';

function arg(name, fallback) {
    const i = process.argv.indexOf(`--${name}`);
    return i === -1 ? fallback : process.argv[i + 1];
}

const PORT = parseInt(arg('port', '5200'), 10);
const ROOT = resolve(arg('dir', 'bench/data'));

const MIME = {
    '.fits': 'application/fits',
    '.fit': 'application/fits',
    '.fz': 'application/fits',
    '.gz': 'application/gzip',
    '.json': 'application/json',
};

function corsHeaders() {
    return {
        'Access-Control-Allow-Origin': '*',
        'Access-Control-Allow-Methods': 'GET, HEAD, OPTIONS',
        'Access-Control-Allow-Headers': 'Range, Content-Type',
        // Without this the browser hides these from fetch(), and a streaming
        // reader cannot tell whether ranges are supported or how big the file is.
        'Access-Control-Expose-Headers': 'Accept-Ranges, Content-Range, Content-Length, Content-Type',
        'Access-Control-Max-Age': '86400',
        // Without this, cross-origin Resource Timing entries report
        // transferSize 0 and the "bytes fetched" measurement is meaningless.
        'Timing-Allow-Origin': '*',
    };
}

/** Parse a single-range `Range: bytes=a-b` header against a known size. */
function parseRange(header, size) {
    const m = /^bytes=(\d*)-(\d*)$/.exec((header || '').trim());
    if (!m) return null;

    const [, rawStart, rawEnd] = m;
    if (rawStart === '' && rawEnd === '') return null;

    let start;
    let end;
    if (rawStart === '') {
        // Suffix form: last N bytes.
        const suffix = Number(rawEnd);
        if (!Number.isFinite(suffix) || suffix <= 0) return null;
        start = Math.max(0, size - suffix);
        end = size - 1;
    } else {
        start = Number(rawStart);
        end = rawEnd === '' ? size - 1 : Number(rawEnd);
    }

    if (!Number.isFinite(start) || !Number.isFinite(end)) return null;
    if (start > end || start >= size) return {unsatisfiable: true};
    return {start, end: Math.min(end, size - 1)};
}

const server = createServer(async (req, res) => {
    if (req.method === 'OPTIONS') {
        res.writeHead(204, corsHeaders());
        res.end();
        return;
    }
    if (req.method !== 'GET' && req.method !== 'HEAD') {
        res.writeHead(405, corsHeaders());
        res.end();
        return;
    }

    const urlPath = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    const path = join(ROOT, normalize(urlPath));
    if (!path.startsWith(ROOT)) {
        res.writeHead(403, corsHeaders());
        res.end('forbidden');
        return;
    }

    let info;
    try {
        info = await stat(path);
        if (!info.isFile()) throw new Error('not a file');
    } catch {
        res.writeHead(404, corsHeaders());
        res.end('not found');
        return;
    }

    const size = info.size;
    const type = MIME[extname(path).toLowerCase()] || 'application/octet-stream';
    const base = {
        ...corsHeaders(),
        'Content-Type': type,
        'Accept-Ranges': 'bytes',
        'Cache-Control': 'no-store',
        'Last-Modified': info.mtime.toUTCString(),
    };

    const range = req.headers.range ? parseRange(req.headers.range, size) : null;

    if (range && range.unsatisfiable) {
        res.writeHead(416, {...base, 'Content-Range': `bytes */${size}`});
        res.end();
        return;
    }

    if (range) {
        const length = range.end - range.start + 1;
        res.writeHead(206, {
            ...base,
            'Content-Range': `bytes ${range.start}-${range.end}/${size}`,
            'Content-Length': length,
        });
        if (req.method === 'HEAD') {
            res.end();
            return;
        }
        createReadStream(path, {start: range.start, end: range.end}).pipe(res);
        return;
    }

    res.writeHead(200, {...base, 'Content-Length': size});
    if (req.method === 'HEAD') {
        res.end();
        return;
    }
    createReadStream(path).pipe(res);
});

server.listen(PORT, () => {
    process.stderr.write(`bench data server: http://localhost:${PORT}/ -> ${ROOT}\n`);
});
