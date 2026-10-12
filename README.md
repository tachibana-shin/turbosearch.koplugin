# turbosearch

Embedded full-text search (phrase + prefix) for KOReader on Kindle Paperwhite 3,
written in Rust. The specification is in [`note.md`](note.md).

## Status

The engine is built and verified against a real library of 619 books
(614 EPUB, 5 MOBI): 49.9 million tokens indexed, and a Vietnamese phrase query
returns 235 hits in about 32 ms. Everything below is measured, not estimated.

| Check | Value |
|---|---|
| Library | 619 books, 12 409 sections, 303.6 MB text |
| Index | 264.8 MB on disk, after compaction |
| Single-word query | ~0.2 ms |
| Phrase query | ~32 ms |
| Full index build | ~9.5 minutes on an x86 host |

## Layout

| Path | What lives there |
|---|---|
| `src/codec/` | Bit-level coders: vbyte, Elias-Fano, PForDelta, BIC, RLE, front coding |
| `src/format/` | Container detection and decoding: EPUB, MOBI/AZW, plain text |
| `src/tokenize/` | Normalisation, diacritic folding, position tracking |
| `src/storage/` | redb tables, scanning, metadata, configuration |
| `src/index/` | Term dictionary, postings codec, entry maps, query path |
| `src/library.rs` | Where the reference library is (see below) |
| `src/bin/` | Measurement and diagnostic tools |
| `tests/` | Integration tests, including ones that need a real library |

## Building

```sh
cargo build --release
```

Cross-compiling for the device:

```sh
rustup target add armv7-unknown-linux-gnueabihf
sudo apt-get install gcc-arm-linux-gnueabihf
CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc \
    cargo check --target armv7-unknown-linux-gnueabihf
```

## Tests

```sh
cargo test                # 302 tests, no library needed
cargo test -- --ignored   # 14 tests that need a KOReader library
```

The tests behind `--ignored` decode real MOBI files, check span boundaries
against real Vietnamese EPUB text, and measure the index against a corpus
nobody hand-picked. They are ignored rather than skipped because a test that
returns early still reports `ok`: a CI run of ten of them would report ten
passes with zero assertions executed.

Point them at a library:

```sh
export TURBOSEARCH_LIBRARY=/path/to/your/calibre
cargo test -- --ignored
```

`TURBOSEARCH_LIBRARY` is the only configuration the tests accept, and it is
deliberately not hardcoded — the default location used to point at one
developer's home directory, which meant every test silently skipped and reported
success for everyone else.

## Tools

```sh
cargo run --release --bin indexer  -- <library> [db]   # build an index, print measurements
cargo run --release --bin breakdown -- <db>            # split the index by codec and by table
cargo run --release --bin latency   -- <db> <phrase>   # query latency over an existing index
cargo run --release --bin grepbook  -- <phrase> <root> # phrase query on extracted text
cargo run --release --bin sniffcheck -- <root>         # what format each file really is
```

`indexer` reads `SEARCH` and `META_SEARCH` from the environment to run a query
after indexing, and `MAX_BOOKS` to index a subset.
