# Baseline — FITS overlay performance before any changes

Measured on `feat/wasm-fits-pipeline` at the fork point, with the stock
`Image.js` → `add_fits_image` → `crop_image` path. Every later phase re-runs
this suite and appends its numbers, so regressions are visible.

## How to reproduce

```sh
npm install
npm run build:dev
npm run bench:data     # writes bench/data/*.fits (~7.6 GB, git-ignored)
npm run bench          # every case
```

Raw JSON per run lands in `bench/results/`.

## Environment

| | |
|---|---|
| Machine | Linux x86_64, 10 cores, 12 GB RAM |
| Browser | Headless Chromium 151 (Playwright 1.62), SwiftShader software GL |
| Toolchain | rustc 1.97.1, wasm-pack 0.13.1, node 22.22.0 |
| Data | synthetic single-HDU FITS, `BITPIX=-32`, TAN WCS, square |
| Server | `bench/serve.mjs` on localhost (Range-capable; Vite answers 500 above ~2 GB) |

Frame times come from a software rasteriser and are pessimistic in absolute
terms. Compare them across runs, not against a real GPU.

## Results — 2026-08-10 (`bench/results/2026-08-10T04-49-02-170Z.json`)

| case | outcome | load | bytes fetched | wasm heap growth | peak JS heap | max frame during load | pan/zoom p95 |
|---|---|---|---|---|---|---|---|
| 64mb | ok | 1.3 s | 64 MiB | 66 MiB | 5 MiB | 138.9 ms | 20.6 ms |
| 512mb | ok | 4.6 s | 512 MiB | 728 MiB | 36 MiB | 319.3 ms | 19.9 ms |
| 1gb | **FAIL** — `recursive use of an object detected which would lead to unsafe aliasing in rust` | 8.1 s | 1024 MiB | 1368 MiB | 1029 MiB | 764 ms | n/a |
| 2gb | **FAIL** — `Image HDU not found in the FITS` | 10.4 s | 2048 MiB | 24 MiB | 36 MiB | 285 ms | n/a |
| 4gb | **FAIL** — `Image HDU not found in the FITS` | 21.5 s | 4096 MiB | 24 MiB | 37 MiB | 288.1 ms | n/a |
| 64mb-x4 | ok | 1.4 s | 256 MiB | 66 MiB | 198 MiB | 170.3 ms | 48.8 ms |
| 64mb-x10 | ok | 2.6 s | 640 MiB | 66 MiB | 262 MiB | 315 ms | 93.8 ms |
| 512mb-x4 | ok | 7.2 s | 2048 MiB | 728 MiB | 1029 MiB | 703.2 ms | 60.2 ms |

## What the numbers say

**The ceiling is between 512 MB and 1 GB, and it is hard.** 512 MB loads; 1 GB
does not. Nothing about the view — zoom level, screen size, how much of the
image is actually visible — changes that, because the whole file is read and
decoded before anything is drawn.

**Above 1 GB the failures are silent and misleading.** At 2 GB and 4 GB the
browser dutifully downloads every byte (4096 MiB fetched) and then the WASM heap
grows by only 24 MiB: the copy into the wasm32 linear memory never succeeds, the
parser is handed nothing, and the user is told `Image HDU not found in the
FITS` — which is not what happened. At 1 GB the allocation does go through
(+1368 MiB) and the failure surfaces instead as a wasm-bindgen re-entrancy
error, so a large load can leave the client in a bad state rather than simply
refusing. Both messages are wrong about the cause; whatever the pipeline ends up
being, it needs an honest "this file is too large to load this way" path.

**Bytes fetched always equals file size.** In every case, including the ones
that fail, the full file crosses the network first. There is no header-only
probe, no range read, and nothing is skipped for being off-screen or below the
resolution the camera can resolve. This is the single largest lever: at
fit-to-screen zoom a 4 GB image needs a few MB of pixels, not 4 GB.

**Memory runs at roughly 1.4× file size inside WASM, on top of the JS copy.**
512 MB of FITS costs 728 MiB of wasm heap — the raw bytes plus the padded
`crop_image` patch buffers — while the JS side still holds the original
`ArrayBuffer`. Two full copies of a large file exist simultaneously, one of them
in a 32-bit address space.

**Many images do not share anything.** 10 × 64 MB loads, but interaction decays
from a 20.6 ms p95 to 93.8 ms — every layer keeps all of its patches resident at
full resolution and is re-rasterised every frame. 4 × 512 MB reaches 1029 MiB of
JS heap and 703 ms main-thread stalls. Scaling to "many FITS, some very large"
needs a shared, evictable, level-of-detail tile cache, not N independent
full-resolution copies.

**Loading blocks the main thread.** Max frame during load tracks file size
directly: 139 ms at 64 MB, 319 ms at 512 MB, 764 ms at 1 GB. Decode and upload
happen inline in the call from JS, so the UI is frozen for that entire window.

## Phase 1 — header-only probe over HTTP ranges

`probeFITS(url)` indexes a file's structure — every HDU, its dimensions, its
position on the sky — by reading headers and stepping over data units
arithmetically. The bytes in between are never requested.

| case | outcome | file size | probe time | bytes fetched | requests | HDUs |
|---|---|---|---|---|---|---|
| probe-64mb | ok | 64 MiB | 28.7 ms | 65836 B | 1 | 1 |
| probe-512mb | ok | 512 MiB | 19 ms | 65836 B | 1 | 1 |
| probe-4gb | ok | 4096 MiB | 16.1 ms | 65836 B | 1 | 1 |

Cost is flat in file size: one request, 64 KiB, regardless. The 4 GB file that
the loading path cannot open at all — after downloading all 4096 MiB of it — is
described completely in 16 ms from 0.0015% of its bytes, WCS included. The
64 KiB is simply the opening probe window; the primary header itself is 2880 B.

Two properties make this work and both carry into the tiling phase: the total
file size arrives in the `Content-Range` of the same request that returns the
first header, so opening a file costs one round trip rather than a `HEAD`
followed by a `GET`; and a server that ignores `Range` is detected from the 200
status and its body reused as an in-memory source, so a non-cooperating origin
is no worse than today's behaviour rather than a second full download.

## Phase 2 — tiled reads with level of detail

`readFITSTiles` reads 512×512 tiles at a chosen level of detail, over range
requests. `zoom` reads the four full-resolution tiles a 1:1 view of the middle
of the image needs; `overview` reads the single top-of-pyramid tile covering the
whole image, under a 16 MiB budget.

| case | outcome | file size | tiles | read time | bytes fetched | bytes used | requests | fraction of file |
|---|---|---|---|---|---|---|---|---|
| tiles-zoom-512mb | ok | 512 MiB | 4 | 318.6 ms | 90 MiB | 4 MiB | 4 | 17.6507% |
| tiles-zoom-4gb | ok | 4096 MiB | 4 | 2472.6 ms | 4 MiB | 4 MiB | 2048 | 0.0982% |
| tiles-overview-512mb | ok | 512 MiB | 1 | 525.4 ms | 16 MiB | 16 MiB | 362 | 3.1209% |
| tiles-overview-4gb | ok | 4096 MiB | 1 | 218.6 ms | 16 MiB | 16 MiB | 128 | 0.3904% |

Byte and request totals here are the reader's own counts, not Resource Timing:
the browser's entry buffer caps out around 250 entries, so a tile needing 512
requests shows up there as far fewer.

A 4 GB image — which the loading path cannot open at all — now yields a
full-resolution view of a region in 2.5 s from 0.1% of its bytes, and a
whole-image overview in 219 ms from 0.39%.

**The byte counts are shaped by row stride, not by cleverness.** A tile is one
byte span per row, and those spans sit `NAXIS1 * bytes_per_pixel` apart. Merging
a run of them costs about `row_stride / row_span` times the useful bytes no
matter how long the run, so the only decision is whether to merge at all, and
the threshold is where a bridged gap costs less than a round trip:
`gap / bandwidth < latency / concurrency`, or 62 KiB at 10 MB/s, 50 ms and 8
requests in flight.

The two zoom rows show both sides of that threshold. The 512 MB image has rows
46 KiB apart, so its tiles merge into one read each — 23 MiB transferred to use
1 MiB, and 75 ms per tile. The 4 GB image has rows 256 KiB apart, past the
threshold, so its tiles stay unmerged and fetch exactly the 1 MiB they need —
but as 512 separate requests, which is why they take 700 ms each. The
apparently wasteful case is the fast one; measuring only bytes would have led
the wrong way here, which is why both `bytesFetched` and `bytesUsed` are
reported.

**The open weakness is request count, not bytes.** A level-0 tile on a
65536-wide image costs 512 requests against an HTTP/1.1 server at 8 concurrent
reads. HTTP/2 multiplexing, more concurrency, and moving the reads off the main
thread (Phase 3) all attack that directly.

**Overviews are bounded by budget, not by file size.** Both overview rows land
at 16 MiB because that is the budget. Reaching it, the sampler thins rows rather
than failing: the 4 GB overview is 512 columns by 128 rows, reported as
`rowThinning: 4`. That is an honest degradation — vertically softer, and it says
so — and it is what keeps a whole-image view affordable when the rows it needs
are scattered across gigabytes. A genuinely cheap overview of a huge file wants
pre-tiling, which is Phase 6.

## Targets for the phases that follow

| | baseline | target |
|---|---|---|
| largest file that loads at all | < 1 GB | 16 GB+ |
| bytes fetched at fit-to-screen zoom | 100% of file | < 1% |
| wasm heap for a large file | ~1.4× file size | bounded by tile budget, independent of file size |
| max main-thread frame during load | 764 ms @ 1 GB | < 16 ms at any size |
| pan/zoom p95 with 10 layers | 93.8 ms | < 20 ms |
| failure mode above the ceiling | wrong error, bad client state | explicit, recoverable |
