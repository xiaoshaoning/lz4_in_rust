# lz4r — the command line tool

`lz4r` compresses and decompresses `.lz4` files, and interoperates with the reference
`lz4` tool in both directions.

**This crate is GPL-2.0-or-later**, unlike the library it drives. It is a port of upstream
`programs/` (`lz4cli.c`, `lz4io.c`), which upstream distributes under GPL-2.0-or-later, so
it lives in its own crate and carries its own `COPYING`. The library at the repository root
(`lz4_in_rust`) stays BSD-2-Clause — the same split upstream uses.

## Build and use

```sh
cargo build --release -p lz4r          # target/release/lz4r
lz4r file.txt                          # -> file.txt.lz4
lz4r -d file.txt.lz4                   # -> file.txt
lz4r -9 -B7 -T0 file.txt               # level 9 (high compression), 4 MiB blocks, auto workers
lz4r -c file.txt | lz4 -d              # pipes, and interop with the reference tool
lz4r -t file.txt.lz4                   # integrity check only
```

Supported: `-1..-12`, `-d`, `-z`, `-c`, `-t`, `-f`, `-q`, `-v`, `-B4..-B7`, `-BD`, `-BX`,
`--no-frame-crc`, `--content-size`, `-T#` (`-T0` = auto, `-T1` = one), `--single-thread`,
`--rm`, `-h`, `-V`, `-` for stdin/stdout, and bundled short flags (`-9f`, `-B4D`). `lz4r -h`
prints the full list. A file holding several concatenated frames is decoded in full.

## What it does not do yet

- **Reads the whole input into memory.** The library's frame layer is whole-buffer, so this
  is not for multi-gigabyte files. Compression of a stream with bounded memory needs the
  `Read`/`Write` adapter (M4c).
- No `-m`/`--multiple` (directories), no `--list`, no `-b` (benchmark), no legacy `-l`
  format, no dictionaries.
- Every level `-1..-12` works and is byte-identical to the reference tool, including its
  block-size shrink for small inputs.

## Tests

`cli/tests/cli.rs` runs the built binary: files, pipes, overwrite protection, `-t` on a
damaged frame, `--rm` only after success, every option combination round-tripping, multiple
frames per file, usage errors, and — when `lz4` is on `PATH` — both interop directions
(ours decoded by `lz4`, and `lz4 -1/-9/-B4/-BD/--content-size` decoded by us).
