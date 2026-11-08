#!/usr/bin/env python3
"""Diff two raw LZ4 blocks as sequence streams, and report the first divergence.

Usage: tools/seqdiff.py <reference.raw> <ours.raw>

Byte comparison only tells you *where* two blocks differ; this says *why*. Each block is
parsed into (literals, offset, match length) runs and the first difference is printed with
its neighbours, which is how every encoder divergence in this port was found (see
docs/REVIEW.md, round 9). Parse errors are reported as such: a block the reference emits
always parses.
"""

import sys


def sequences(block):
    """Parse a raw LZ4 block into (literals, offset, match_len) runs.

    The last run carries `offset=None`: it is the closing literal run.
    """
    ip = 0
    out_pos = 0
    runs = []
    while ip < len(block):
        token = block[ip]
        ip += 1
        lit_len = token >> 4
        if lit_len == 15:
            while True:
                b = block[ip]
                ip += 1
                lit_len += b
                if b != 255:
                    break
        literals_at = ip
        ip += lit_len
        out_pos += lit_len
        if ip >= len(block):
            runs.append((out_pos, lit_len, None, None, literals_at))
            break
        offset = block[ip] | (block[ip + 1] << 8)
        ip += 2
        match_len = token & 15
        if match_len == 15:
            while True:
                b = block[ip]
                ip += 1
                match_len += b
                if b != 255:
                    break
        match_len += 4
        runs.append((out_pos, lit_len, offset, match_len, literals_at))
        out_pos += match_len
    return runs


def describe(run):
    out_pos, lit_len, offset, match_len, _ = run
    if offset is None:
        return f"out {out_pos}: final literals {lit_len}"
    return f"out {out_pos}: literals {lit_len}, offset {offset}, match {match_len}"


def main():
    if len(sys.argv) != 3:
        print(__doc__.strip())
        return 2
    ref_path, our_path = sys.argv[1], sys.argv[2]
    ref = sequences(open(ref_path, "rb").read())
    ours = sequences(open(our_path, "rb").read())

    print(f"reference: {len(ref)} sequences, {len(open(ref_path, 'rb').read())} bytes")
    print(f"ours     : {len(ours)} sequences, {len(open(our_path, 'rb').read())} bytes")

    for i, (r, o) in enumerate(zip(ref, ours)):
        if r != o:
            print(f"\nfirst divergence at sequence {i}:")
            for j in range(max(0, i - 2), min(len(ref), i + 2)):
                mark = ">>" if j == i else "  "
                print(f"{mark} ref  {describe(ref[j])}")
                print(f"   our  {describe(ours[j]) if j < len(ours) else '(none)'}")
            return 1

    if len(ref) != len(ours):
        print(f"\nsequences agree up to {min(len(ref), len(ours))}, then the lengths differ")
        return 1
    print("\nidentical sequence streams")
    return 0


if __name__ == "__main__":
    sys.exit(main())
