#!/usr/bin/env python3
"""Generate synthetic FITS images of arbitrary size for benchmarking.

Writes a valid single-HDU image with a TAN WCS, streaming row blocks so that
multi-gigabyte files can be produced without holding them in memory.

    ./gen_fits.py --size 4GB --out bench/data/synth-4gb.fits

The pixel pattern is deterministic (a smooth gradient plus a lattice of
Gaussian sources plus reproducible noise) so that renders can be eyeballed for
correctness and compared between runs.
"""

import argparse
import os
import re
import sys

import numpy as np

BLOCK = 2880
BITPIX_DTYPE = {8: ">u1", 16: ">i2", 32: ">i4", -32: ">f4", -64: ">f8"}

SIZE_UNITS = {"": 1, "B": 1, "KB": 1 << 10, "MB": 1 << 20, "GB": 1 << 30, "TB": 1 << 40}


def parse_size(text):
    m = re.fullmatch(r"\s*([0-9.]+)\s*([A-Za-z]*)\s*", text)
    if not m:
        raise argparse.ArgumentTypeError(f"cannot parse size: {text!r}")
    unit = m.group(2).upper()
    if unit not in SIZE_UNITS:
        raise argparse.ArgumentTypeError(f"unknown size unit: {m.group(2)!r}")
    return int(float(m.group(1)) * SIZE_UNITS[unit])


def card(key, value, comment=""):
    """Format one 80-character FITS header card."""
    if isinstance(value, bool):
        rendered = "T" if value else "F"
    elif isinstance(value, str):
        rendered = f"'{value:<8}'"
    elif isinstance(value, float):
        rendered = f"{value:.10E}"
    else:
        rendered = str(value)

    body = f"{key:<8}= {rendered:>20}"
    if comment:
        body = f"{body} / {comment}"
    if len(body) > 80:
        raise ValueError(f"card too long: {body!r}")
    return f"{body:<80}"


def build_header(width, height, bitpix, crval1, crval2, fov_deg):
    # Degrees per pixel so that the image spans roughly fov_deg across.
    cdelt = fov_deg / width

    cards = [
        card("SIMPLE", True, "conforms to FITS standard"),
        card("BITPIX", bitpix),
        card("NAXIS", 2),
        card("NAXIS1", width),
        card("NAXIS2", height),
        card("BSCALE", 1.0),
        card("BZERO", 0.0),
        card("CTYPE1", "RA---TAN"),
        card("CTYPE2", "DEC--TAN"),
        card("CRPIX1", width / 2.0),
        card("CRPIX2", height / 2.0),
        card("CRVAL1", crval1),
        card("CRVAL2", crval2),
        card("CDELT1", -cdelt),
        card("CDELT2", cdelt),
        card("CUNIT1", "deg"),
        card("CUNIT2", "deg"),
        card("EQUINOX", 2000.0),
        card("RADESYS", "ICRS"),
        card("ORIGIN", "gen_fits"),
    ]
    if bitpix in (16, 32):
        cards.append(card("BLANK", -32768 if bitpix == 16 else -2147483648))
    cards.append(f"{'END':<80}")

    header = "".join(cards)
    pad = (-len(header)) % BLOCK
    return (header + " " * pad).encode("ascii")


def dims_for(target_bytes, bitpix, aspect):
    """Pick width/height whose data unit is closest to target_bytes."""
    bytes_per_px = abs(bitpix) // 8
    total_px = target_bytes / bytes_per_px
    height = int(round((total_px / aspect) ** 0.5))
    width = int(round(height * aspect))
    # FITS itself has no alignment requirement, but round to 16 so that tiling
    # and SIMD lanes divide evenly and benchmarks are not measuring ragged edges.
    return max(16, width - width % 16), max(16, height - height % 16)


def write_data(fh, width, height, bitpix, rows_per_block, seed):
    dtype = np.dtype(BITPIX_DTYPE[bitpix])
    rng = np.random.default_rng(seed)

    x = np.arange(width, dtype=np.float32)
    # A lattice of Gaussian sources every 512 px, so any crop of the image has
    # recognisable structure at several zoom levels.
    spacing = 512.0
    sigma = 12.0
    src_x = np.exp(-(((x % spacing) - spacing / 2) ** 2) / (2 * sigma**2))
    gradient_x = x / max(1, width - 1)

    written = 0
    for y0 in range(0, height, rows_per_block):
        y1 = min(y0 + rows_per_block, height)
        y = np.arange(y0, y1, dtype=np.float32)[:, None]

        src_y = np.exp(-(((y % spacing) - spacing / 2) ** 2) / (2 * sigma**2))
        gradient_y = y / max(1, height - 1)

        block = 0.35 * (gradient_x[None, :] + gradient_y)
        block += 3.0 * (src_x[None, :] * src_y)
        block += rng.normal(0.0, 0.02, size=block.shape).astype(np.float32)

        if bitpix == 8:
            out = np.clip(block * 64.0, 0, 255).astype(dtype)
        elif bitpix == 16:
            out = np.clip(block * 8000.0, -32767, 32767).astype(dtype)
        elif bitpix == 32:
            out = np.clip(block * 500000.0, -2147483647, 2147483647).astype(dtype)
        else:
            out = block.astype(dtype)

        fh.write(out.tobytes())
        written += out.nbytes

        done = y1 / height
        sys.stderr.write(f"\r  {done * 100:5.1f}%  {written / (1 << 20):.0f} MiB")
        sys.stderr.flush()

    sys.stderr.write("\n")

    pad = (-written) % BLOCK
    if pad:
        fh.write(b"\0" * pad)
    return written


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--size", type=parse_size, default=parse_size("64MB"),
                   help="approximate data-unit size, e.g. 512MB or 4GB")
    p.add_argument("--bitpix", type=int, default=-32, choices=sorted(BITPIX_DTYPE),
                   help="FITS BITPIX (default -32, 32-bit float)")
    p.add_argument("--aspect", type=float, default=1.0, help="width / height")
    p.add_argument("--fov", type=float, default=2.0, help="image width in degrees")
    p.add_argument("--ra", type=float, default=83.822, help="CRVAL1 in degrees (default: Orion)")
    p.add_argument("--dec", type=float, default=-5.391, help="CRVAL2 in degrees")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--out", required=True)
    args = p.parse_args()

    width, height = dims_for(args.size, args.bitpix, args.aspect)
    header = build_header(width, height, args.bitpix, args.ra, args.dec, args.fov)

    os.makedirs(os.path.dirname(os.path.abspath(args.out)), exist_ok=True)

    # ~64 MiB of float32 scratch per block keeps peak RSS bounded regardless of
    # the output size.
    rows_per_block = max(1, (64 << 20) // (width * 4))

    print(f"{args.out}: {width} x {height}, BITPIX={args.bitpix}", file=sys.stderr)
    with open(args.out, "wb") as fh:
        fh.write(header)
        data_bytes = write_data(fh, width, height, args.bitpix, rows_per_block, args.seed)

    total = len(header) + data_bytes
    print(f"  wrote {total / (1 << 20):.1f} MiB", file=sys.stderr)


if __name__ == "__main__":
    main()
