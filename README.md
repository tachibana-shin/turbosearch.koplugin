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
| `tests/koreader-probe/` | The reader-inspection probe, a KOReader plugin in its own right |
| `script/` | The Teal compiler fetcher, the release-index writer, and the uinput helper |
| `build/` | Generated, not committed: the plugin, and one release archive per platform |

## Building the engine

```sh
cargo build --release
```

Cross-compiling goes through `cross`, which puts each target in a container
carrying that target's own sysroot and linker. One way to ask for a target and
get the same answer everywhere, rather than whichever cross gcc happened to be
installed:

```sh
make plugin PLATFORM=kindlea9
```

`cross` needs a container engine. podman works, and the Makefile sets
`CROSS_CONTAINER_ENGINE=podman` explicitly rather than leaving it to be guessed.
The resulting `.so` needs only `libc`, `libpthread` and `libdl` — SQLite is
compiled in, so there is no `libsqlite3` to install on the device.

## The KOReader plugin

The plugin searches the library from inside the reader. One popup, in the order a
reader works through it — what am I looking for, what kind of search is it, how
wide a search, build, go:

```
┌─ Search in books ──────────────────┐
│ the last query                     │
│                                    │
│ ☐ Metadata only                    │
│ ☐ Regular expression               │
│ ☐ Search the whole library         │
│                                    │
│ [⚙ Settings] [↻ Index] [🔍 Search] │
└────────────────────────────────────┘
```

Each box is a separate index operation rather than a strength setting, so more
than one can be ticked and the last one wins — which is said on the label rather
than left to be discovered. The boxes start where the reader left them last time,
because a reader who looks for an author once looks for another author next.

**Where you are is what is searched.** With a book open it is that book's folder;
on the main menu it is the folder the browser is showing; with neither there is
nothing narrower to ask about and the search covers the library. The third box is
how a reader in a folder asks about every book instead, and it starts off.

The search builds what it is about to answer from, so a folder that has never been
indexed is indexed on the way to asking about it — no separate step, and nothing
to remember. What is already indexed costs one `stat` per file and no writes, so
the second search in a folder costs what the first did. The whole library is
minutes the first time and seconds after that, so that one is asked about first:
the scan says how many books are still to read before a single one is opened, and
the confirm box shows it. **Index** is the same work offered by itself, for a
reader who knows they are about to look in this folder and would rather pay for
it once than once per query.

A tap opens the book. The results menu carries each book's title and its format
on the right, where the reader's own file browser puts it.

**Settings** (left of the group, or the second menu entry) is one screen holding
every setting the engine acts on, in two panes — not a report behind a button
behind a popup:

```
┌─ ⚙ Book search settings ─────────┐
│ Stored — what the engine acts on  │
│ Folders to index  /mnt/us/…       │
│ File types to skip log,lua,js…    │
│ Positions kept per term   full    │
│ Expand a prefix query     auto    │
│ What is stored           full    │
│                                  │
│                              [✕] │
└──────────────────────────────────┘
```

A row is a label on the left and what is in force on the right, measured against
the screen rather than counted in characters — a character is eight pixels on one
device and twelve on another, and only the pixels decide whether the label still
has room. Tap a row to change it. Closing comes back to the keyword, because a
settings screen is a round trip and not a destination.

The two panes are two different homes for a setting:

- **Stored** is the record inside the index, which the engine reads back the next
  time anything opens it: what to index and skip, the size bounds, how many
  positions to keep, how a prefix query expands, what is stored at all.
- **Next run** is what the indexer was started with, and a settings screen cannot
  reach into a running process — so it records what the *next* run should use, in
  `<index>.env` beside the index. A variable already set in the environment wins
  over that file, so a one-off run from a shell still behaves.

`What is stored` is read from the engine but refused on the way back. It decides
what is stored, so changing it would leave postings in the shape the old mode
produced and every result would come from an index that no longer means what it
says; the refusal names the fix, which is to rebuild. There is no case-sensitivity
setting, and not by omission: the index stores folded terms, so a query never had
a case to be sensitive to, and giving it one would mean storing the original
alongside every term — doubling an index whose size the whole design is built
around.

```sh
make plugin                   # build the plugin for this machine
make plugin PLATFORM=kindle   # or for the Kindle, or for aarch64
make install                  # point KOReader at it
make packages                 # one release archive per platform, in build/
```

There is no `plugin/` directory in the repository. Everything the reader loads —
the plugin's Lua, its metadata, and the engine library — is generated into
`build/turbosearch.koplugin/` from the Teal sources in `tl/`, and the whole
directory is a build output:

```sh
make plugin                   # build for this machine
make plugin PLATFORM=kindlea9 # or any platform in the table below
make install                  # symlink it into ~/.config/koreader/plugins
```

`make install` links it the way the other plugins on the machine are linked, so it
is one directory on disk rather than a copy that has to be kept in step. Keeping
the plugin directory under version control, with Lua in it that is also generated
from Teal, put the generated files one commit away from the sources they are
compiled from.

One engine library per platform sits in that directory, and the plugin picks
between them from `jit.arch`. An x86-64 library reaching a Kindle fails at
`ffi.load` with a message about the ELF class, so choosing by name is what keeps
that from being a search that returns nothing rather than a load that says why.

A whole library is still built outside the reader: minutes and hundreds of
megabytes are not a menu callback, and the confirm box exists so a reader is asked
before those minutes are spent.

```sh
make build && cargo run --release --bin indexer -- <library> <db>
CONTENT_MODE=full cargo run --release --bin indexer -- <library> <db>  # phrase queries
```

Point the plugin at the index in the reader's settings for the plugin. What comes
after that is the plugin's own — a folder indexed from inside it, a book that
changed and was read again, a book that was deleted and is no longer an answer —
through the same stepped session the binary uses, which is why an index built by
one is an index the other keeps in step.

## Lua toolchain

The Lua side is written in Teal — Lua with static types, compiling to plain Lua
that LuaJIT runs — because the C ABI is where a wrong type costs a use-after-free
rather than a Lua error.

```sh
make lua-check      # type-check the Teal sources
make lua-build      # compile them into the plugin directory
make lua-test       # the plugin against stubbed KOReader widgets
make help           # every target
```

`script/tl` pins a compiler version and its checksum, and downloads it into
`~/.cache/` on first use, so the repository carries the recipe and the machine
carries the binary. `tl/ffi.tl` declares the slice of `require("ffi")` the
engine uses — without it `tl check` reports `module not found` on plain Lua and
then fails on every use of ffi, a check that cannot fail for a real reason.

Everything the plugin touches is declared once, in `tl/`, against a stub of the
reader's own module: `Menu`, `InputDialog`, `ButtonDialog`, `ConfirmBox`,
`CheckButton`, `InfoMessage`, `WidgetContainer`, `UIManager`, `LuaSettings`,
`Dispatcher`, `FileManager`, and — because the settings screen sizes a column in
pixels rather than in characters — `device`, `ui/font` and `ui/rendertext`. That
is what turns a field the installed reader does not have into a build error rather
than a screen that silently does not do what it looks like — `dimmed`,
`UIManager:setInputDialog` and `ui/widget/alertdialog` were all found on a device,
one at a time, and would have been found here. `MODE` and the engine's ABI are
checked the same way.

What Teal does *not* catch: a case-insensitive setting, because the index stores
folded terms — a query never had a case to be sensitive to, and giving it one
would mean storing the original beside every term and doubling an index whose
size the whole design is built around.

### Verifying against a real reader

`script/lua-test` stubs the KOReader widgets. The one thing it cannot reach is
the installed reader itself, so `tests/koreader-probe/` closes that gap: it builds
the same `InputDialog` and `Menu` the plugin builds, from the rows a real search
returns, reads and refuses a setting through the same calls the settings screen
uses, and opens a book through `FileManager.openFile`. It is inert unless asked
for, so installing it costs nothing — and `TURBOSEARCH_PROBE=settings` leaves the
settings screen open and photographs both of its panes, which is how the column
that was squeezing a label to zero width was found.

```sh
ln -s "$PWD/plugin/turbosearch-probe.koplugin" \
      ~/.config/koreader/plugins/turbosearch-probe.koplugin
TURBOSEARCH_PROBE=1 /usr/lib/koreader/koreader.sh 2>&1 | grep PROBE
```

On a 619-book-corpus index built from 614 EPUBs, that printed 33 hits for a
Vietnamese phrase, 13 metadata matches, 33 rows with every title resolved, the
real `Menu` accepted, and a book opened in 0.275 s.

## Releases

`make packages` builds the engine for each platform with `cross`, and packs a
separate archive per platform into `build/`. Each holds the plugin's Lua, its
metadata, and the one engine library that platform needs — because the
alternative, one archive carrying every architecture, ships libraries that are
dead weight on any given reader.

| Platform | Target | For |
|---|---|---|
| `desktop` | `x86_64-unknown-linux-musl` | the desktop reader, and any x86-64 machine |
| `kindle` | `arm-unknown-linux-musleabi` | Kindle, soft-float ABI, no SoC tuning |
| `kindlea9` | `arm-unknown-linux-musleabi` + `target-cpu=cortex-a9` | **Kindle Paperwhite 3** |
| `kindlehf` | `arm-unknown-linux-musleabihf` | Kindle whose toolchain uses the hard-float ABI |
| `aarch64` | `aarch64-unknown-linux-musl` | newer readers with a 64-bit ARM |

Every Linux target is musl rather than gnu. A `.so` linked against the build
machine's glibc is linked against a version the reader does not have, and the
failure is a reader that loads the plugin and then finds no engine; musl carries
its own. The Paperwhite 3 is soft-float even on ARMv7, which is why `kindlea9`
and `kindle` share a target and `kindlea9` exists only for the SoC tuning.

`build/turbosearch.json` indexes them: which archive is which platform, the target
it was built for, its size, and a sha256. Attached to a release so a script — or a
reader deciding whether to redeploy — can tell what it has without unpacking
anything.

Releases are semantic-release, driven by commit messages, attaching one archive
per platform (`.releaserc.yaml`, shaped after the rakuyomi plugin's).

## Tests

```sh
make test                 # 318 tests, no library needed
make test-library         # 18 tests that need a KOReader library
make lua-test             # the plugin, against stubbed KOReader widgets
make check                # everything CI runs
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
