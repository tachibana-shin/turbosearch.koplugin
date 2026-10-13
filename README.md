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
| Index, `full` mode | 163.1 MB on disk, stable across rebuilds |
| Index, `terms` mode | 31.8 MB |
| Index, `files` mode | 0.9 MB |
| Single-word query | ~0.2 ms |
| Phrase query | ~32 ms |
| Full index build | ~114 s on an x86 host |
| Rescan of an unchanged library | 3.8 s, and 0 bytes written |

The three `ContentMode`s exist for the same reason the phrase query is optional:
a Kindle has neither the space for 163 MB nor the patience for 114 s, and
`files` and `terms` cost 0.9 MB / 15 s and 31.8 MB / 97 s respectively.

## Layout

| Path | What lives there |
|---|---|
| `src/codec/` | Bit-level coders: vbyte, Elias-Fano, PForDelta, BIC, RLE, front coding |
| `src/format/` | Container detection and decoding: EPUB, MOBI/AZW, plain text |
| `src/tokenize/` | Normalisation, diacritic folding, position tracking |
| `src/storage/` | SQLite schema, config, metadata, book records |
| `src/index/` | Term dictionary, postings codec, entry maps, query path |
| `src/sql/` | The sqlx engine: connection pool, migrations, compaction |
| `src/ffi.rs` | The C ABI the Lua side binds to |
| `src/library.rs` | Where the reference library is (see below) |
| `src/bin/` | Measurement and diagnostic tools |
| `tl/` | Teal sources for the Lua binding, plus `ffi.tl` |
| `plugin/` | The KOReader plugin: Lua, engine library, `.so` |
| `migrations/` | The only schema definition; also drives sqlx's compile-time checks |
| `tests/` | Integration tests, including ones that need a real library |

## Building the engine

```sh
cargo build --release
```

Cross-compiling for the device:

```sh
rustup target add armv7-unknown-linux-gnueabihf
cargo zigbuild --target armv7-unknown-linux-gnueabihf --release
```

`cargo zigbuild` rather than an arm gcc cross-compiler, because there is no
`arm-linux-gnueabihf-gcc` on every machine that has `zig`; both produce the same
ELF 32-bit ARM EABI5 object, and the resulting `.so` needs only `libc`,
`libpthread` and `libdl` — SQLite is compiled in, so there is no
`libsqlite3` to install on the device.

## The KOReader plugin

The plugin searches the library from inside the reader: a query box prefilled
with the last query, a results menu carrying each book's title, and a tap opens
the book. Beside **Search** in that box is a **Settings** button, and there is a
second menu entry for it, so a reader can find out what an index holds without
dismissing a query first.

The settings screen reports the index it points at, its size on disk including
the write-ahead log, its term count and its content mode, and lets the reader
choose what the query means:

| Mode | Reads the query as |
|---|---|
| `Text` | book text, with `"quoted"` phrases matching exactly |
| `Metadata` | title, author and series only |
| `Pattern` | a regular expression over the terms in the index |

A content mode that cannot do what the chosen mode asks is said out loud rather
than discovered by typing: a `files` index has no text to search, and a `terms`
index has nothing for a phrase to match.

There is no case-sensitivity setting, and not by omission. The index stores
folded terms, so a query never had a case to be sensitive to; giving it one would
mean storing the original alongside every term, doubling an index whose size the
whole design is built around.

```sh
script/build-plugin           # host library, for the desktop reader
script/build-plugin --arm     # armv7 library, for the device
```

The two libraries sit in `plugin/turbosearch.koplugin/` side by side and the
plugin picks between them from `jit.arch`, so an x86-64 build cannot end up on a
Kindle pretending to work. To use the plugin, symlink it the way the other
plugins on the machine are:

```sh
ln -s "$PWD/plugin/turbosearch.koplugin" \
      ~/.config/koreader/plugins/turbosearch.koplugin
```

The plugin only reads an index — building one takes minutes and writes hundreds
of megabytes, which is not something to do from a menu callback:

```sh
cargo run --release --bin indexer -- /path/to/library /path/to/turbosearch.db
CONTENT_MODE=full cargo run --release --bin indexer -- <library> <db>  # phrase queries
```

Point the plugin at it in the reader's settings for the plugin.

## Lua toolchain

The Lua side is written in Teal — Lua with static types, compiling to plain Lua
that LuaJIT runs — because the C ABI is where a wrong type costs a use-after-free
rather than a Lua error.

```sh
script/lua-check    # type-check the Teal sources
script/lua-build    # compile them into the plugin directory
script/lua-test     # run the plugin against stubbed KOReader modules
./script/tl --version
```

`script/tl` pins a compiler version and its checksum, and downloads it into
`~/.cache/` on first use, so the repository carries the recipe and the machine
carries the binary. `tl/ffi.tl` declares the slice of `require("ffi")` the
engine uses — without it `tl check` reports `module not found` on plain Lua and
then fails on every use of ffi, a check that cannot fail for a real reason.

### Verifying against a real reader

`script/lua-test` stubs the KOReader widgets. The one thing it cannot reach is
the installed reader itself, so `plugin/turbosearch-probe.koplugin` closes that
gap: it builds the same `InputDialog` and `Menu` the plugin builds, from the rows
a real search returns, and opens a book through `FileManager.openFile`. It is
inert unless asked for, so symlinking it in costs nothing.

```sh
ln -s "$PWD/plugin/turbosearch-probe.koplugin" \
      ~/.config/koreader/plugins/turbosearch-probe.koplugin
TURBOSEARCH_PROBE=1 /usr/lib/koreader/koreader.sh 2>&1 | grep PROBE
```

On a 619-book-corpus index built from 614 EPUBs, that printed 33 hits for a
Vietnamese phrase, 13 metadata matches, 33 rows with every title resolved, the
real `Menu` accepted, and a book opened in 0.275 s.

## Tests

```sh
cargo test                # 318 tests, no library needed
cargo test -- --ignored   # 18 tests that need a KOReader library
script/lua-test           # the plugin, against stubbed KOReader widgets
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
after indexing, `CONTENT_MODE` to pick how much to store, `FRESH` to discard the
existing index, and `MAX_BOOKS` to index a subset.
