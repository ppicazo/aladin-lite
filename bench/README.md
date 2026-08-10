# FITS benchmark harness

Measures what large FITS overlays actually cost: time to a visible image, bytes
pulled off the network, WASM and JS heap growth, and how long the main thread is
blocked while it happens.

## Generate test data

`bench/data/` is git-ignored; regenerate it locally.

```sh
python3 bench/gen_fits.py --size 64MB  --out bench/data/synth-64mb.fits
python3 bench/gen_fits.py --size 512MB --out bench/data/synth-512mb.fits
python3 bench/gen_fits.py --size 4GB   --out bench/data/synth-4gb.fits
```

Files carry a valid TAN WCS centred on Orion and a deterministic pattern — a
smooth gradient plus a 512 px lattice of Gaussian sources plus seeded noise — so
renders can be eyeballed at any zoom and compared between runs. `--bitpix`
selects the pixel type (`8`, `16`, `32`, `-32`, `-64`; default `-32`).

Requires `numpy` only. Generation streams in row blocks, so peak RSS stays near
64 MiB no matter how large the output is.

## Run

```sh
npm run build:dev          # bench imports src/core/pkg, so the wasm must exist
node bench/run.mjs                        # every case
node bench/run.mjs --cases 64mb,512mb     # a subset
node bench/run.mjs --headed               # watch it run
```

The runner boots the Vite dev server on port 5199, drives
`examples/al-bench-fits.html` in Chromium once per case, and writes
`bench/results/<timestamp>.json` plus a Markdown table on stdout.

A case that fails — out of memory, allocation failure, timeout — is recorded
with its failure mode rather than aborting the run. At large sizes that failure
*is* the measurement.

Cases named `probe-*` measure the header-only path instead: they call
`probeFITS(url)` and report bytes fetched and request count, which should stay
flat as file size grows.

## Debugging a case

`bench/debug-probe.mjs` loads the probe page once with every console message,
page error and failed request echoed to the terminal:

```sh
node bench/debug-probe.mjs http://localhost:5200/synth-4gb.fits
```

## Interactive

Open the page directly to poke at a single file:

```
/examples/al-bench-fits.html?file=/bench/data/synth-512mb.fits&count=4&pan=3000
```

`count` stacks that many independent layers of the same file, which is the
multi-image case. `pan` is the milliseconds spent driving the camera afterwards
to measure interaction frame times.

## Caveats

- `peak JS heap` comes from `performance.memory`, which is Chrome-only and
  excludes ArrayBuffers allocated outside the JS heap. Treat it as a lower
  bound; `wasm heap growth` is the reliable number.
- Headless Chromium renders through SwiftShader, so absolute frame times are
  pessimistic. Compare runs against each other, not against a real GPU.
- The dev server is local, so network time is effectively zero. Bytes fetched is
  the meaningful network number here, not duration.
