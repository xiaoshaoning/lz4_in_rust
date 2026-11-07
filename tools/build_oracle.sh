#!/bin/sh
# Build the C reference oracle and (re)generate the interop fixtures.
#
#   tools/build_oracle.sh [path-to-c-lz4-repo]
#
# Default C repo: ../lz4 (D:\Projects\github\lz4). Output: target/c-oracle,
# fixtures: tests/fixtures/*.lz4 + *.raw  (committed, so `cargo test` needs no C toolchain).
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
crepo=${1:-$(cd "$here/../lz4" && pwd)}
out=$here/target/c-oracle
mkdir -p "$here/target" "$here/tests/fixtures"

cc -O2 -I"$crepo/lib" -o "$out" "$here/tools/c_oracle.c" \
   "$crepo/lib/lz4.c" "$crepo/lib/lz4hc.c" "$crepo/lib/xxhash.c"
echo "built $out"

gen() {  # gen <input> <level>   (level<=0 -> fast, else HC) -> <input>-l<level>.lz4
    "$out" block "$2" "$here/tests/fixtures/$1.raw" "$here/tests/fixtures/$1-l$2.lz4"
    printf '%-16s raw=%-7s lz4=%s\n' "$1-l$2" \
        "$(wc -c < "$here/tests/fixtures/$1.raw")" \
        "$(wc -c < "$here/tests/fixtures/$1-l$2.lz4")"
}

# fixture inputs, one per code path worth exercising
cd "$here/tests/fixtures"
: > empty.raw
printf 'a' > one.raw
printf 'abcabcabcabcabcabcabcabcabcabcabcabc' > short_text.raw
head -c 4096 /dev/zero > zeros.raw
awk 'BEGIN{for(i=0;i<512;i++) printf "line %d: the quick brown fox\n", i}' > text.raw
head -c 20000 /dev/urandom > random.raw
awk 'BEGIN{for(i=0;i<8000;i++) printf "%d\n", (i*2654435761)%251}' > numbers.raw
cd "$here"

for f in empty one short_text zeros text random numbers; do gen "$f" 0; done
gen text 9
gen numbers 9
gen text 12
