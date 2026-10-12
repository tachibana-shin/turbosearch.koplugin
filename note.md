lib.rs
```rs
//! turbosearch — embedded full-text search for KOReader.
//!
//! The engine is built in layers; the codec layer is complete first because every
//! other layer's size and latency depends on it.
//!
//! ```text
//! codec/      column coders (Elias-Fano, BIC, RLE, front coding)  [phase 1]
//! storage/    redb schema and transaction handling                 [phase 2]
//! format/     sniffers and decoders per container format           [phase 3]
//! tokenize/   normalization, CJK handling, positions                [phase 3]
//! index/      incremental scan, extraction, posting blocks          [phase 3]
//! query/      phrase, prefix expansion, ranking, snippets          [phase 5]
//! ffi/        C ABI surface loaded from LuaJIT                      [phase 6]
//! ```
//!
//! The crate is a `lib` so it can later be built as `cdylib` for the LuaJIT FFI;
//! the `turbosearch` binary is a thin CLI over the same code.
```

# TURBOSEARCH — TECHNICAL SPECIFICATION v4

**Full-Text Search Engine (phrase + prefix) for KOReader on Kindle Paperwhite 3, written in Rust.**

> This document completely supersedes v1 in the appendix at the end of the file.
> v2 added: decision Q1 (option a), lol-html, spine order, no `book_id` reuse.
> v3 added: the **English-only convention for code** (§1.3) and the **maximum compression strategy** (§3.4).
> v4 added: **book metadata indexing** (§2.3, §3.2) and **multi-format support + file extension configuration** (§3.0, §3.5).
> All changes are listed in §0.

---

## 0. Change log

### v1 → v2

| # | v1 | v2 | Reason |
|---|---|---|---|
| 1 | DB < 3% of the library (~50MB / 2.5GB) | **Accept ~20–25%** + disk budget (§1.2) | 3% is unreachable with a positional index — §1.1 |
| 2 | Hand-written state machine (InTag/InText) | **`lol-html` only** | `>` inside attributes, `<!-- -->`, CDATA, entities, UTF-16 shift positions and inject garbage into the index |
| 3 | Read ZIP entries in `read_dir` order | Read in **OPF `<spine>`** order | Positions must match reading order, otherwise snippets show up in the wrong place |
| 4 | Stopword filtering | **Index every word**; soft cap of 100K pos/(book, term) | A larger size is already accepted → exact adjacency (= 1), phrases like "the girl with…" match correctly |
| 5 | Deleting a book = cleaning up old postings | **No `book_id` reuse**; orphan postings discarded at query time; periodic compaction | Deleting one book must not scan 600M postings → O(1) |
| 6 | Delta + VByte | PForDelta (later upgraded in §3.4 v3) | Better compression, ~2–4× faster decode on Cortex-A9 |
| 7 | `inode + mtime + size`, dir check via mtime | `path + size + mtime`; `inode` is only a hint; **drop** the dir-mtime check | `/mnt/us` on Kindle is **vfat**: inodes are not stable and mtime has only **2-second** resolution |
| 8 | Return "byte/word position", `seek()` into the raw file | `entry_idx + byte_off` + `ANCHORS`; xpointer **lazy**; snippet decoded with `lol-html` | KOReader opens books via `Locator`; `seek()` into raw HTML would expose tags |
| 9 | Whitespace-splitting tokenizer | **Fold diacritics (NFD → strip combining marks)** + **bigram for CJK** | "tham tu" must produce "thám tử"; Chinese/Japanese have no spaces |
| 10 | None | **Prefix search** (dictionary range scan) + **tf×idf ranking** | E-ink keyboard UX; `book_id` ordering is meaningless |
| 11 | Not mentioned | **Resume**: cursor + 1 transaction per book | PW3 has weak battery; the initial index takes 10–30 minutes |
| 12 | EPUB only | Clear scope: **EPUB/TXT/FB2** phase 1, **MOBI/AZW3** phase 2, **KFX/CBZ/DJVU/PDF/DOCX not indexed** | Kindle libraries contain many AZW3 files |
| 13 | None | `META` holds the schema/tokenizer/normalizer version + `config_hash` | Changing one normalization line turns the entire old index into permanent garbage |
| 14 | Not mentioned | `cdylib` + C ABI, loaded via LuaJIT FFI, cross-compile `armhf` (§5) | KOReader is LuaJIT + C |
| 15 | None | Cap on entries / total uncompressed size / free space check | EPUB is untrusted input (zip bomb); a full disk damages the device |

### v2 → v3 (this update)

| # | v2 | v3 | Reason / Savings |
|---|---|---|---|
| 16 | Not specified | **English-only for all code, comments, logs, errors, test names, commits** (§1.3) | Errors surfacing in KOReader logs must be greppable; rustc/clippy/rustdoc are all English; ready for external contributors |
| 17 | "redb can compress" (implicit) | **redb 2.x has NO native compression** — verified in the docs: `Builder` only has `set_cache_size`, `set_page_size`, `set_repair_callback`, `set_region_size`. All compression must be implemented ourselves (§3.4 L5) | Avoid the trap: waiting for redb to compress = an entirely uncompressed index |
| 18 | Postings contain `byte_off: u32` | **Drop `byte_off` from postings**; replace it with `ENTRYMAP.checkpoints` — a sparse `(pos, byte_off)` array every 2048 positions (§3.4 L1) | **~15–20%** — this is the highest-entropy field in a posting |
| 19 | `TERMS` = one KV per term (~1M KVs) | **`DICT` = a single KV**, front-coded + zstd, also holding df/tf/chunk_count; `DICT_DELTA` for the running session (§3.4 L4) | **~30–50MB** of B-tree/page overhead + prefix search becomes a binary search in RAM |
| 20 | PForDelta with 4 columns | **Elias-Fano** (doc gap) + **Binary Interpolative Coding** (pos gap) + **RLE** (entry_idx), then **zstd-19 + trained dictionary** on top (§3.4 L2–L3) | **~10–20%** vs PForDelta |

### v3 → v4 (this update)

| # | v3 | v4 | Reason |
|---|---|---|---|
| 21 | Index book content only | **Index book metadata too** (title, author, series, tags, publisher, description…) via the `METADATA` + `METAIDX` tables (§2.3, §3.2) | Users look for books by name/author/genre, not only by content |
| 22 | EPUB support only (TXT/FB2/AZW3 deferred to phase 2) | **Multi-format from phase 1**: EPUB/KEPUB, MOBI/AZW/AZW3, TXT/MD, HTML/XHTML, FB2 (+`.fb2.zip`) | Real Kindle libraries hold many `.azw3`/`.mobi` files; skipping them means skipping most books |
| 23 | None | **Configurable**: `include_extensions` / `exclude_extensions` / `exclude_dir_globs` / `source_dirs` (§3.5) | Users add the extensions they want scanned and exclude junk extensions themselves |
| 24 | None | **Format detection by magic byte**, the extension is only a fast pre-filter (§3.0) | `.az`/`.azw`/`.azw3`/`.kfx` are named haphazardly in practice; renamed files must still be indexed correctly |
| 25 | Snippets extract book text only | A match in metadata returns a **metadata tag** (title/author/series) instead of a snippet | Results of a title search must show book information, not a paragraph of text |
| 26 | None | A `CONFIG` table stores all user settings; clearly separate the options that **change the token stream** (→ rebuild) from those that **only change the set of files scanned** (→ rescan) (§3.5.3) | Changing a sensitive config without rebuilding = a silently wrong index |
| 27 | `.az` treated as "like EPUB" | **Correction**: MOBI/AZW/AZW3 is **PalmDB + PalmDOC LZ77**, *not* ZIP. The payload inside is HTML (as in EPUB), but the container is completely different (§3.0.2) | Writing a parser for the wrong container = nothing can be read |
| 28 | Queries match full terms only | **Fallback prefix expansion** (§4.1.1): a term absent from `DICT` is expanded into a prefix | Typing `cha` and not getting `chai` — on a PW3 keyboard, typing part of a word is the dominant behavior, not an edge case |
| 29 | None | `prefix_search_mode` (`auto`/`exact`/`prefix`) + a `fuzzy` flag + surfacing to the user that the term was expanded | Without that signal the user cannot understand why typing `cha` returns books that do not contain the string "cha" |
| 30 | All size figures are estimates (±50%) | **Real measurements on 614 EPUBs** (§1.1.1): 1,36GB EPUB → 348,7MB text → 56,3M token → **full positional index 102,9MB (7,6% EPUB)** | Gate §7.7 has run. The old crude extrapolation inflated by ~5,8× because it guessed token/library density wrong |
| 31 | The proposed "2 tiers + verification" architecture out of fear the index would be too large | **Not needed** — full positional is only 102,9MB. Only necessary above a library of > ~3,5GB | Do not add complexity when the measurements show it is unnecessary |
| 32 | `zstd-19` for the build path | **`zstd` level 3** (§3.4 L3): 6,5s vs 512s on 348,7MB, only 32% bigger than level 19 | Level 19 on Cortex-A9 costs 1,5–3 hours per build |
| 33 | redb "2.x" | **redb 4.3.0** (API verified): `Builder::set_cache_size()`, key/value of `(&[u8], u16)`, **no native compression** | The crate has moved to 4.x; `set_cache_size` exists only on `Builder`, `len()` only on *untyped* tables, and `iter().next()` returns `Option<Result<..>>` |
| 34 | "reading a table that was never created = an error" | **`TableDoesNotExist` = an empty table**, handled centrally in `storage::or_empty()` | A newly created DB or a user who has not enabled indexing is a normal state, not an error |
| 35 | A single scan across every root | **Two modes: `scan_scope(dir)` and `scan_full(config)`**, `ScanMode::{Scope,Full}` (§3.1.1) | The user types a search while browsing a subdirectory → index just that directory (a few seconds), not the whole library (several minutes) |
| 36 | Search always over the entire DB | **`ScopeFilter(dir)`** limits results by path prefix | Searching in a subdirectory must not mix in books from other directories |
| 37 | `missing` books evaluated over the entire DB | Only evaluate books under roots **readable in this pass** | Without this guard, a full scan of one root wipes out books scope-indexed under another root |
| 38 | Use the `mobi` crate 0.8.0 to parse MOBI | **Defer MOBI/AZW/AZW3 for now** | The `mobi` crate (the only one) has not been updated since 2022-12, depends on `encoding` 0.2 (abandoned), has 21,9% doc coverage, and `content_as_string()` returns a single String → **the byte offsets** needed by `ENTRYMAP.checkpoints` are **lost**. Only 5/614 volumes (0,8%) |
| 39 | Sniffing reads the first 64 bytes | Read **128 bytes** | The `BOOKMOBI` magic sits at offset 60..68 — reading 64 bytes cuts the magic in half and every MOBI becomes `Unknown` |
| 40 | Compare `<!doctype html` byte-for-byte | Compare **case-insensitively** | `<!DOCTYPE html>` is the standard, but real files contain `<!Doctype Html>`, `<!doctype html>` |
| 41 | Parse `Event::Start` only | Parse both `Event::Start` **and** `Event::Empty` | `<item/>` and `<itemref/>` in the OPF are **self-closing** → parsing only `Start` leaves **a completely empty spine** and loses the reading order of every EPUB |
| 42 | Extract text with the `text!("body")` handler | Use the `element!("*")` handler + `remove_and_keep_content()` | The `text!` handler also captures text **inside** `<script>` (a child of `body`) → JavaScript leaks into the index |
| 43 | An `&` without `;` keeps at most 32 bytes | 3-state machine: `Complete` / `NotEntity` / `Truncated` | `Tom & Jerry` has `&` followed by a space → the literal must be kept immediately, otherwise a single `&` swallows 32 bytes |
| 44 | Trust the manifest's `media-type` to know which entries are content | **Trust the file extension**: `.xhtml`, `.html`, `.htm`, `.xhtm` (§3.2) | `media-type` is often missing or misrecorded in real EPUBs; the extension in `href` is always present |
| 45 | Compare OPF element/attribute names **case-sensitively** | `local_name()` and `attr_value()` compare with **`eq_ignore_ascii_case`** | HTML/XHTML does not require any particular case — `<Title>`, `<ItemRef>`, `<OPF:SCHEME>`, `IDREF` are all valid |
| 46 | Look up ZIP entries **case-sensitively** | `read_entry` falls back to a case-insensitive scan | An href of `ChapterOne.XHTML` does not match the entry `chapterone.xhtml` → the section is **silently** dropped |
| 47 | `<?xml`, `<![CDATA[` compared case-sensitively | `eq_ignore_ascii_case` | Real files contain `<?XML`, `<![cdata[` |
| 48 | No `tokenize` module — whitespace survives in the text | A `tokenize` module with `for_each_token()` (§3.2.1) | `\r`, `\n`, `\t`, nbsp must all act as separators; previously they were still inside `Section.text` |
| 49 | NFD + dropping combining marks only | Add manual folding of `đ`/`Đ` → `d` | `đ` (U+0111) is a letter of its own and NFD does **not** split it — without folding, typing `duoc` will not find `được` |
| 50 | `is_alphanumeric()` keeps letters and digits in one run | Split letters from digits | `Chapter12` must yield `chapter` + `12` so that searching `3` finds `Tập 3` |
| 51 | One-way diacritic folding → previews are lost | `for_each_token_spanned()` returns byte ranges in the **original text** (§3.2.3) | `OffsetMap` maps normalized bytes back to original bytes |
| 52 | `src` advanced per char during NFD | Advance by the **original char**, with every byte produced by that char mapping back to the same `char_start` | A span landing mid-character → slice panic. NFD of U+1EE7 yields 3 chars, so accumulation ends at byte 57 in the middle of the letter `ủ` |
| 53 | **Flush postings after every book** → one chunk per (term, book) | Accumulate **across books**, writing only when the pool exceeds `MAX_PENDING` (4M postings) | Real library: 743.587 chunks / 62.663 terms → 492MB output. After the fix: **71.619 chunks** — exactly enough for each term to hit `CHUNK_POSTINGS` |
| 54 | Initially unaware that redb retains free pages | `Engine::compact()` after the session has closed its writer | Real measurement: a freshly written file is 293MB but the **raw data is only 91,5MB** → 69% is dead pages. Compaction is mandatory, not optional |
| 55 | Read chunks in **key** order | Sort by the decoded `Chunk.first_doc` | Key `(term, chunk_idx: u16)` — redb encodes u16 **big-endian**, so a term with >256 chunks iterates out of order (chunk 256 comes before chunk 1) → positions go backwards and phrase queries **silently lose hits**. Caught by the real-library test |
| 56 | Restart position 0 for **every section** | A single counter spanning the whole book, with `entry_idx` distinguishing sections | `pos_gap` is a delta: resetting per section makes positions **duplicate** within the same document → underflow → chunk decode corrupted. Symptom: search returns 0 hits across the board |
| 57 | A hand-written PalmDOC parser got 3 rules wrong | `0x01..=0x08` = count N literals from the stream (does not repeat the previous character); the 14-bit item `0x80..=0xBF` = `((b&0x3F)<<8)\|next` → dist `item>>3`, len `(item&7)+3`; `0xC0..=0xFF` emits `' ' + (b^0x80)` | Caught by hexdumping a real file: `0xEF` → `" o"`, `0xC3` → `" C"`. Verified: all 5 MOBI files decode cleanly, with Vietnamese diacritics correct |
| 58 | The indexer did not write `BOOKS` | Write `BOOKS` for every book at the same time as its metadata | `rebuild_index()` only covers books with a live `BOOKS` row → a missing row makes metadata **silently disappear** from every metadata search. Postings and metadata must be assigned the same book_id |
| 59 | MOBI has no metadata path | `from_embedded()` reads EXTH for Mobi/Azw/Azw3 via `format::mobi` | Users require metadata to be indexed for every format; title/authors can be obtained without decompressing the text |
| 53 | `.nfd()` applied to the whole string | NFD **per character**, and **exempting Hangul syllables** | Hangul decomposes into jamo (not combining marks) → `한국어` falls apart, and per-char NFKC cannot recompose it |
| 54 | Intended to drop positions because "preview still has to search the EPUB again" | **Keep exact positions** (§3.4 L1.2) | Searching again is a **seek**, not a scan: 3ms (1 section) vs 18ms (the whole book). Without positions, finding "tham tu" means extracting every candidate book ≈ 9 seconds |
| 55 | Spans could land inside a multi-byte character | Test `real_book_slices_are_always_on_char_boundaries` over 40 real books | Unit tests cannot reproduce it; it only appears on real text |
| 56 | `remove_book` deletes only `BOOKS` + `PATHS` | Also delete the `METADATA` row, and let `rebuild_index` take only ids still in `BOOKS` | `METAIDX` is rebuilt from `METADATA` rows → a leftover row makes a deleted book **come back to life in every metadata search, permanently**. Orphan postings are filtered out on read; orphan metadata is not |
| 57 | `MetaIndex::push` required the caller to normalize | `push` and `search` **normalize themselves** | Forgetting it once → the index silently matches nothing, with no error reported |
| 58 | FB2 `sequence` was an `Event::Empty` | Merge `Start | Empty` (the second occurrence of the same bug) | The bug recurred a third time after OPF `itemref`/`item`: self-closing elements were dropped entirely, this time losing **every FB2 series** |
| 59 | Trust `media-type` to know which entries are content | **Trust the file extension** (§3.2.2) | `media-type` is often missing or wrong in real EPUBs; `href` is always present |
| 60 | OPF elements/attributes compared case-sensitively | `eq_ignore_ascii_case` | `<Title>`, `<ItemRef>`, `OPF:SCHEME`, `IDREF` are all valid → metadata and spine are both lost |
| 61 | Preview takes the span of the last token | Remember `candidate_start` from the first token | `push()` completes the match and only receives the span of the **last** token → the preview returns `"tử"` instead of `"thám tử"` |

#### 3.2.1 The tokenize module — the places that are easy to get wrong

The `tokenize` module implements §3.2 steps 4–10. Three non-obvious decisions:

**① `đ` cannot be split by NFD — it must be folded manually.** `đ` (U+0111) is a
**letter of its own**: NFD returns the letter itself, there is no decomposition. If you rely on NFD alone, a user typing `duoc` on a US keyboard will **never** find `được`.
`fold_letter()` maps `\u{0110}`/`\u{0111}` → `d` separately.

> **`đ` and `ơ` are fundamentally different** — verified in code, not assumed:
> `NFD('ơ') = ['o', '\u{31B}']` — the horn is a **combining mark with its own code point**, so it
> is removed by an ordinary NFD pass. `NFD('đ') = ['đ']` — the crossbar **has no decomposition at all**. The same mechanism
> applies to `ư` (u + U+031B), `ă` (a + U+0306),
> `â ê ô` (a/e/o + U+0302).
>
> The test `every_vietnamese_letter_collapses_to_one_ascii_letter` scans the whole Vietnamese
> alphabet (14 dedicated letters + U+1EA0..U+1EF9) and confirms that **no** character is left in a non-ASCII form. Remove `fold_letter` and `đ`/`Đ` show up immediately in that list.

**Two consequences that must be recorded (these are not bugs):**

- **Dropping tone marks makes distinct words collide.** `ta tá tà tả tã tạ` (ta "I", tá "slap", tà "fall",
  tả "left", tã "nappy", tạ "thanks") all collapse into `ta`. In Vietnamese, tone has distinguishing meaning.
  This is the price of §0 #9 (so that people without a Vietnamese keyboard can still find books).
  The correct fix is to index **both forms** (folded + original), but that doubles
  the term dictionary.
- **Northern European languages are affected too.** `å ä ö` **do** decompose (base letter + combining mark)
  so they are folded the same way: `år` (year) and `ar` collide. Only `ø æ ß ł ð þ` do
  not decompose, so they stay as they are. This stays invisible on a Vietnamese library, but you must
  know about it in advance.

**② Letters and digits split apart (`Chapter12` → `chapter`, `12`).** `char::is_alphanumeric()`
returns `true` for both, so a single plain run would glue them together. A user searching for
`3` must get `Tập 3` back — so a run that switches class (letters ↔ digits) is cut off and flushed.

**③ Every character that is not a word-char is a separator.** Specifically `\r`, `\n`,
`\t`, `\u{000B}`, `\u{000C}`, `\u{00A0}` (nbsp), `\u{2028}`, `—`, `_`, `-`,
`'`, punctuation. No token can contain whitespace; `term_is_clean()` checks
this.

The normalization order is **mandatory**: NFD → strip marks → `\u{0111}` fold → lowercase → NFKC.
Run NFKC first and the string is recomposed and the combining marks can no longer be recognized.

Position is a monotone counter over the **entire book**, continuous across sections
(`for_each_token` returns the position for the next chunk). CJK is counted **per character**,
not per byte: `中文测试` → unigram at 0,1,2,3 and bigram at 0,1,2.

Verified on a real library (`bin/tokcheck`): 25 books → **3.091.495 tokens,
0 bad tokens**, ~123K tokens/book, ~550K tokens/second.

#### 3.2.3 Phrase query — verified on a real library

The pivotal question: *does "thám tử" match a document that records "Thám tử lừng danh"
?"* **Yes.** The test `tests/phrase_query.rs` runs the full pipeline (extract → tokenize
→ positional intersect) on 120 real books:

| Query | Matching books | Preview (verbatim) |
|---|---|---|
| `thám tử` (with tone marks) | 64 | `"thám tử"` |
| `Thám tử` (different case) | 64 | `"thám tử"` |
| `THÁM TỬ` (all caps) | 64 | `"thám tử"` |
| `tham tu` (**no tone marks**) | 64 | `"thám tử"` |
| `Thám-tử` (different separator) | 64 | `"thám tử"` |
| `tử thám` (reversed order) | 29 | `"từ thâm"` |
| `thám` (single word) | 120 | `"thấm"` |
| `thám tử lừng danh` (3 words) | 8 | **`"thám tử lừng danh"`** |
| `sherlock holmes` | 14 | `"Sherlock Holmes"` |
| nonsense | 0 | — |

Four properties confirmed by data, not by argument:

1. **Dropping tone marks does not change the result set** — 64 == 64. This is the core requirement.
2. **Case has no effect** — the same 64 for all 3 spellings.
3. **Separators between words have no effect** — `Thám-tử` returns the same result, because
   `-` is only a separator and the positions stay adjacent.
4. **Order is meaningful** — `tử thám` returns 29 books, noticeably fewer.

Two notable observations from the table:
- The query `thám` matches books that write **`thấm`** — exactly the tone-dropping consequence already noted
  in §3.2.1 (two different words collapse onto one key). This is concrete evidence
  of that known cost.
- Preview is always the **verbatim text of the book**, never the folded key.

Matching runs streaming (O(1) memory) instead of holding the entire token stream: one book
has ~124K words, and the first version of this test materialized them into separate `String`s and
took 5 minutes; the streaming version takes **15 seconds**.

#### 3.2.5 Provenance — verified on a real library

Priority order (§2.3): KOReader sidecar → embedded metadata → file name.

Verified on a real library: **43/43** books in the 150-book sample have a sidecar
`metadata.epub.lua`, so the sidecar almost always decides. This is not a
minor detail: some OPFs in the library record `<dc:language>en</dc:language>` for
Vietnamese-language books, while the sidecar records `text_lang = vi`. Trust the OPF and search is
wrong — the sidecar is what the user is actually looking at, so it must win.

The sidecar is Lua, but we **do not** embed a Lua engine: we only read the subset that
KOReader writes (`["key"] = value`, one level of nesting, strings with line-continuation).
Running arbitrary Lua from the book directory is a far greater risk than the value of the feature.

#### 3.2.6 The result preview is not lost

A legitimate concern: tone folding + lowercase is a **one-way loss**, so can we still
display "Thám tử" when the user types "tham tu"?

Keep the distinction sharp:

| Component | State |
|---|---|
| Original text in the book | **Never overwritten** — `Section.text` keeps case and tone marks intact |
| Indexing key | Normalized (`"Nguyễn"` → `"nguyen"`) |
| Token position | Yes |
| **Token offset in the original text** | Yes, via `for_each_token_spanned()` |

`for_each_token_spanned()` returns an extra `(start, end)` — a byte range into the **original text**,
which is exactly what is needed to cut the snippet window. `note.md` §4.5 describes how to use it:
take the position → find the span → cut ±7 tokens around it → display the verbatim text.

Two bugs caught while building this part (see §0 #52, #53):
- `src` was advanced by **char during NFD** → the span landed in the middle of a character and the slice panicked.
- `.nfd()` tears **Korean** apart into jamo and it cannot be recomposed.

A real example: query `"nguyen"` → key `"nguyen"` → preview
`"Thẩm phán, công tố viên và nguyên đơn đứng ở một khoảng cách khá"` — the correct
verbatim text with tone marks, not the folded key.

#### 3.2.2 The "trust the file extension" rule

A manifest entry counts as **content** if:

```
if href ends in .xhtml / .html / .htm / .xhtm   -> content           (case-insensitive)
otherwise if href has no extension             -> consult media-type
otherwise                                      -> not content
```

The last branch is the essential guard: with no extension **and** no valid `media-type`, images,
CSS and fonts can never slip into the index. `media-type` is only a fallback, and must never override
the file extension.
| 35 | `config_hash` includes `min_file_size` | Includes only the setting that changes the **token stream** (`index_depth`); every setting that changes the **set of files scanned** stays out | `min_file_size` only affects which books get scanned → rescan, not rebuild |



---
## 1. Goals & Constraints

### 1.1 Decision Q1: choose **option (a)** — full positional index

#### 1.1.1 Real measurements (already run — gate §7.7, no longer estimates)

Measured over **all 614 EPUBs** in `~/.config/koreader/rakuyomi/downloads/calibre`,
without extrapolating (`cargo run --release --bin measure`):

| Item | Measured value |
|---|---|
| `.epub` folder on disk | **1,36 GB** |
| Text after extraction (raw UTF-8) | **348,7 MB** (25,6% of EPUB) |
| Token = posting (including stopword) | **56,3 million** |
| Distinct terms | 70.600 |
| Dictionary (front-coded) | **401 KB** |
| **Full positional index** | **102,9 MB = 7,6% of EPUB** |

Build performance (x86 host, to infer the order of magnitude on PW3):
- Extract 614 books: **63 s**
- Encode full index: **5,2 s**

**Conclusion: option (a) is entirely feasible.** With a 1,36GB library, the full index is only 102,9MB
on the ~2,7GB user partition of the PW3. The only condition is that the library not exceed
**~3,5GB** (102,9MB × 3,5/1,36 ≈ 265MB), still within the §1.2 budget.

Effect of keeping positions (`cap` = max number of positions per (term, book)):

| cap | positions kept | Index | % EPUB |
|---|---|---|---|
| **full** | 56,3 M (100%) | **102,9 MB** | 7,6% |
| 32 | 18,8 M (33%) | 39,5 MB | 2,9% |
| 8 | 8,5 M (15%) | 20,1 MB | 1,5% |
| 2 | 3,2 M (6%) | 8,7 MB | 0,6% |

**Default: `cap = full`.** The "2-tier + verification" architecture I once proposed is **not necessary**
for this library — it only becomes worthwhile when the library exceeds 3,5GB. `cap` is still user-configurable
(`index_depth`) so that users with large libraries can lower it themselves to 32 or 8.

#### 1.1.2 Old estimate figures refuted by measurement

| Assumption in the old version | Real measurement | Consequence |
|---|---|---|
| 2,5GB library → 600M tokens | 1,36GB → 56,3M tokens (`~24 byte EPUB/token`, not ~4) | The old extrapolation inflated sizes ~5,8× |
| `pos_gap` ~5,5 bit/posting | **11,70 bit/posting** | No coder (BIC/PFor/EF) can beat it: the average distance between two occurrences is ~3300 tokens |
| `doc_gap` ~2,5 bit/posting | **0,99 bit/posting** | Cheaper than guessed, because the posting bulk concentrates on high-frequency terms (dense doc column) |
| `zstd-19` for the build path | **zstd-19: 512 s / 348MB** on host; `zstd-3`: **6,5 s** | zstd-19 on the PW3 would take several hours → **use zstd-3** (§3.4 L3) |

**Why `pos_gap` accounts for 84% of the index:** position must be stored absolutely within a long span
(~92 thousand tokens/book), so each gap carries ~11,7 bit entropy. This is a floor of the phrase
query problem itself, not a weakness of the compressor.

- No truncating book content, no block-index → **phrase query 100% accurate**.
- **Stopwords are indexed** (if stopwords were filtered, "to be or not to be" would never match).
- The DB occupies ~7,6% of the library for the measured library → very comfortable (§1.2 is still kept
  as a buffer in case of a much larger library).

### 1.2 Disk budget (mandatory, not optional)

```
FREE_SPACE_FLOOR = 256 MB      # KOReader needs room for cache/screenshot/temp
DB_BUDGET       = min(25% × library_bytes, 700 MB)
# measured library: 1.36 GB × 25% = 340 MB >> the actual 102.9 MB -> never comes near the ceiling
```

- Before each book: if `free_space < FREE_SPACE_FLOOR` or `db_size + est_next_book > DB_BUDGET` → **stop indexing**, set `status = budget_exceeded`.
- UI displays: "% of library indexed", "DB currently occupies X MB", button to disable indexing.
- Priority order when the budget is about to run out: **most recently added first** (`ORDER BY mtime DESC`).
- Further option if it is still too tight: the `index_depth` setting — `full` (default) / `cap=32` / `cap=8` (§1.1.1). This is a **starvation safeguard** deliberately enabled by the user, not a default. For the measured library, `full` is only 102,9MB, so almost nobody needs to touch it.
- Optional setting: `index_only_when_charging` (on by default — avoids draining the battery).

### 1.3 Source code convention: **English only**

**All code, comments, doc-comments (`///`), identifier names, module names, log messages, error messages, test names, and commit messages must be written in English.**

**Only `note.md` and `docs/*.md` are in Vietnamese.**

Reasons:
- Our error messages go straight into the KOReader log (users paste it when reporting bugs) → they must be greppable/lookup-able.
- The output of `rustc`, `clippy`, `rustdoc`, `cargo`, and every backtrace is in English; mixing Vietnamese into the log makes tracing confusing.
- LuaJIT FFI is a C language boundary — function names and descriptions must be readable from both sides.
- If we later gain outside contributors or open-source the crates.

How it is applied:
- Vietnamese string literals in `src/` are forbidden (checked in review).
- `error`/`log` messages follow the pattern: `"turbosearch: <subsystem>: <what happened>: <actionable detail>"`.
- CI runs `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`; the review checklist has one line "no non-English strings in src/".
- Variable names like `book_id`, `doc_gap`, `entry_idx` — use standard English information-retrieval terminology, **not** translated into Vietnamese and then abbreviated.

```rust
// Good
/// Sparse map from token position to byte offset in the raw entry.
/// Checkpoints are emitted every CHECKPOINT_INTERVAL positions.
pub struct Checkpoints { pub interval: u32, pub entries: Vec<(u32, u32)> }

// Bad — never do this
/// Checkpoint table: token position -> byte offset within the raw entry
```

### 1.4 Performance goals (measured on real hardware, not guessed)

| Metric | Goal |
|---|---|
| Scan metadata of 6.000 files | < 50 ms |
| Index 1 EPUB of 400KB | < 800 ms (heavier than v2 because of compression) |
| Index the entire 2.5GB library (first time) | 15–40 minutes, resumable |
| Phrase query of 2–3 words | p50 < 10 ms, **p95 < 50 ms** |
| Query with prefix expansion (§4.1.1) | p95 < 80 ms — wider budget because more terms must be decoded (cap of 100K postings total) |
| RAM resident at query time | < 40 MB (of which dictionary ~10–15MB) |

---

## 2. Data model (redb 2.x)

10 tables. All values are **hand-written byte packing** (no serde/bincode), so that changing the schema is a deliberate act.

```rust
const META:       TableDefinition<&str,        &[u8]> = TableDefinition::new("meta");
const BOOKS:      TableDefinition<u16,         &[u8]> = TableDefinition::new("books");
const PATHS:      TableDefinition<&[u8],       u16>   = TableDefinition::new("paths");
const METADATA:   TableDefinition<u16,         &[u8]> = TableDefinition::new("metadata");
const METAIDX:    TableDefinition<&str,        &[u8]> = TableDefinition::new("metaidx");
const DICT:       TableDefinition<&str,        &[u8]> = TableDefinition::new("dict");
const DICT_DELTA: TableDefinition<&[u8],       &[u8]> = TableDefinition::new("dict_delta");
const POSTINGS:   TableDefinition<(&[u8], u16), &[u8]> = TableDefinition::new("postings");
const ENTRYMAP:  TableDefinition<(u16, u16),  &[u8]> = TableDefinition::new("entrymap");
const CONFIG:     TableDefinition<&str,        &[u8]> = TableDefinition::new("config");
```

### META — a single entry (`key = "meta"`)
```
schema_version:     u32 = 3
tokenizer_version:  u32 = 1
normalizer_version: u32 = 1
codec_version:      u32 = 1     # version of the compression block §3.4
config_hash:        u32        # hash of the config that affects the index
book_id_hwm:        u16        # next id to be assigned
dict_size_estimate:u32
db_bytes_written:  u64
status:             u8         # idle | scanning | indexing | budget_exceeded
scan_cursor:       [u8]       # most recently processed path (resume)
```
Any version or `config_hash` that differs from the current build → **delete the DB and rebuild from scratch** (safer than trying to read an old format).

### BOOKS — key `book_id: u16`
```
scope_len: u16, scope: [u8]     # root that discovered this book (§3.1.1)
path_len: u16, path: [u8]
size: u64, mtime: i64, inode: u64      # inode is for debugging only, not used for matching
format: u8                              # epub|txt|fb2|azw3
entry_count: u16
token_count: u32
indexed_at: i64
flags: u8                               # bit0 = partial (capped), bit1 = orphan
```

### PATHS — key `path` → `book_id:u16`
O(1) reverse lookup for scanning.

### METADATA — key `book_id: u16` → value
Descriptive information about the book, used to **display** results. Not used for matching.

```
title:        [u8]                     # as in the book, preserving diacritics and case
subtitle:     [u8]
authors:      [ [u8] ]                 # already split, deduplicated
creators:     [ [u8] ]                 # editor, translator, illustrator, narrator…
language:     [u8]                     # BCP-47 lowercase, "vi", "en", "zh-CN"
publisher:    [u8]
year:         i32 (0 = unknown)
series:       [u8]
series_index: u32 (×1000, 0 = unknown)
identifiers:  [ (scheme:u8, value:[u8]) ]   # isbn, doi, uuid, asin, uri, goodreads
tags:         [ [u8] ]                 # subject/genre/keyword
description:  [u8]                     # cap 4 KB, set the `desc_truncated` bit if truncated
word_count:   u32
source_format: u8                      # Format enum, §3.0.1
provenance:   u8                       # embedded | sidecar | filename
flags:        u8                       # bit0 = desc_truncated, bit1 = meta_only
```

**Provenance matters:** it determines where metadata is taken from, in priority order:
1. **KOReader sidecar** (`<book>.sdr/metadata.epub.lua`) if present → this is what the user **sees** in the KOReader library, so reusing it keeps things consistent. It needs a mini parser for a *subset* of flat Lua tables (`key = "string"`, `key = 123`, `key = true`) — ~80 lines, not a Lua engine. *(Optional, phase 3 — if implemented, well worth it.)*
2. **Metadata embedded in the file** (EPUB OPF / MOBI EXTH / FB2 `<description>`).
3. **File name** (drop the extension, split on `-`/`_`) — for `.txt`/`.mobi` with no metadata.

### METAIDX — **a single KV** (`key = "metaidx"`)
**This is the table that actually serves metadata search.** The reason postings are not used for metadata: postings can only match a **whole token** (or a prefix), while metadata needs **substring** matching — the user typing `nguy` must yield `Nguyễn`, typing `imov` must yield `Asimov`. Substring matching on an inverted index is impossible.

```
[n_books: u32]
for each book: [book_id: u16][blob_off: u32][blob_len: u16]     # index parallel to the blob
[blob…]          # normalized blob (same normalizer as §3.2), concatenated, zstd-19 over the whole block
```

Contents of each book's blob (already normalized, separated by `\n`):
```
title\nauthor1\nauthor2\nseries\ntag1\ntag2\npublisher\nyear\nfilename_stem\ndescription
```

**Why a single KV instead of 6000 KVs:** reading 6000 individual blobs = 6000 `get()` calls = 6000 B-tree page traversals. On a Kindle that is 100–300 ms. Merging into one KV of ~1.5–2MB compressed, loading it into RAM once (LRU 1), then scanning with `memmem` → **< 3 ms** for the entire library (6000 × ~300 bytes = 1.8MB, linear scan with SIMD).

`METAIDX` is **rewritten in full once per indexing session** (compressing 2 MB takes ~100 ms — far cheaper than maintaining a delta table).

### DICT — **a single KV** (`key = "dict"`)
The entire term dictionary, **front-coded + zstd**.

```
[n_terms: u32] = N
[m_index_entries: u32]
for each block of 32 terms (front-coded):
  each term: [u8 prefix_len][u8 suffix_len][u16 df][u32 tf][u16 chunk_count][u8 flags][suffix bytes]
```
The `m_index_entries` header is a sparse index for binary search: each entry points to the first block containing a term ≥ a given threshold, inside the **decompressed** region. The build script generates the sparse index so that entries are ~64 entries apart.

Why one KV instead of 1M KVs (§3.4 L4): redb stores each KV pair in 4KB leaf pages. ~1M terms × (key 10B + value 12B + overhead) ≈ 30–50MB just for page overhead. A single ~5MB compressed KV eliminates this entirely, and prefix search becomes a binary search in RAM.

### DICT_DELTA — key `term` → small value
Contains only the changes from the currently running indexing session: `df_delta`/`tf_delta`/`chunk_count`. When the session closes (or compacts), merge into `DICT` and **rewrite `DICT` exactly once**, then delete `DICT_DELTA`. This avoids rewriting the whole dictionary (several MB compressed) after every book.

Prefix search while merging: probe both `DICT` and `DICT_DELTA` (the delta is small and cheap).

### POSTINGS — key `(term, chunk_idx: u16)` → value
```
[u16 first_book_id]     # binary-search the chunk without needing an auxiliary index table
[u32 codec_tag]         # magic + version of the compression block
[bytes nén]             # §3.4 L1..L3
```
**Chunk of ~8192 postings (~7–8KB compressed).** Small chunks are intentional: `get()` in redb reads the entire value → small chunks allow lazy decoding, cheap binary search, and eliminate the 4KB page.

**Key property:** `book_id` increases monotonically (high-water-mark). Ids are never reused → re-indexing a book always produces an id **greater** than every old id → the new chunk always sorts after old chunks by `(book_id, pos)`. **Append-only therefore does not break the global ordering**, and two intersecting cursors still run linearly. Orphaned postings sit inside the sequence and are skipped during intersection.

### ENTRYMAP — key `(book_id, entry_idx)` → value
Combines the role of `ANCHORS` (v2) and the new checkpoint table:
```
entry_name: [u8]
byte_len:   u32
xpath_hint: [u8]              # stable id/class if present, to build a Locator quickly
checkpoints: [u8]             # delta+PFor: one (pos, byte_off) pair every 2048 tokens
```

### CONFIG — user settings (§3.5)
```
source_dirs:           [ [u8] ]    # default ["/mnt/us/documents"]
include_extensions:    [ [u8] ]    # §3.5.1
exclude_extensions:    [ [u8] ]    # §3.5.1
exclude_dir_globs:     [ [u8] ]    # §3.5.2
min_file_size:         u64         # default 5120
max_file_size:         u64         # default 64 MiB
index_fields:          u32 bitmask # which metadata fields go into the METAIDX blob
longtail_formats:      u8          # enable the ebook-rs crate: RTF/ODT/DOCX/LIT/KFX
meta_only_unparseable: u8          # PDF/DJVU still index metadata (off by default)
prefix_search_mode:    u8          # auto (default) | exact | prefix  — §4.1.1
```

All CONFIG changes go through the Lua side → FFI `config_set()` / `config_get()`. Rust does not read config files and does not hard-code any KOReader paths.

> **§2.7 The "single KV" table rule** (`META`, `DICT`, `METAIDX`): load into RAM on demand, LRU 1. They differ in **data** and **rewrite cadence**: `META` is written on every scan (a few hundred bytes), `DICT` merges once per session, `METAIDX` is rewritten once per session. All three are outside the query hot path once loaded.

---

## 3. Pipeline

### 3.0 Format detection & file extension filtering

Two steps, deliberately different — **do not confuse them**:

```
step 1  extension pre-filter  ->  decides whether to READ the file at all (cheap, never opens it)
step 2  magic sniff (64 byte) ->  decides the REAL format (opens the file, reads 64 bytes)
```

**Extensions are only a speed pre-filter; magic bytes are the referee.** Reason: in a real Kindle library, `.azw`/`.azw3`/`.mobi`/`.kfx` are named inconsistently, and files are frequently renamed with the wrong extension. If we trusted extensions alone, we would silently drop an entire group of books without reporting any error.

#### 3.0.1 Format enum & magic

| Format | Magic / signature | Text source | Metadata source | Phase |
|---|---|---|---|---|
| **EPUB2/3, KEPUB** | `PK\x03\x04` + entry `mimetype` = `application/epub+zip` | `.xhtml`/`.html` entries in OPF `<spine>` order → `lol-html` | OPF `<metadata>` | 1 |
| **MOBI / AZW (KF7) / AZW3 (KF8)** | `BOOKMOBI` at offset **60..68** of the PDB header | decompress PalmDOC LZ77 → HTML → `lol-html` | **EXTH records** (updated title, creator, publisher, description, subject, isbn, language) | 2 |
| **TXT / MD** | no magic, guessed: valid UTF-8 & high ratio of printable characters | whole file | file name + `Title:` header (if present) | 1 |
| **HTML / XHTML / MHTML** | starts with `<` (after stripping BOM/whitespace) | whole file → `lol-html` | `<title>`, `<meta name="author">` | 1 |
| **FB2, `.fb2.zip`** | `<?xml` + root `<FictionBook` | `<body>` | `<description>/<title-info>` (title, author, genre, annotation, date, lang) + `<publish-info>` | 2 |
| **DOCX** | ZIP + entry `word/document.xml` | — | `docProps/core.xml` | 3 |
| **ODT** | ZIP + `mimetype` = `application/vnd.oasis.opendocument.text` | `content.xml` | `meta.xml` | 3 |
| **RTF** | `{\rtf` | — | `{\info …}` | 3 |
| **LIT** | `ITOLITLS` | — | — | 3 |
| **KFX (KF10)** | container magic `CONT` | — | — | 3 |
| **PDF / AZW4 (Print Replica = wrapped PDF)** | `%PDF` | — | — | do not index content |
| **DJVU** | `AT&TFORM` | — | — | do not index content |
| **CBZ / CBR / CB7** | all-image ZIP | — | — | do not index content |

> ⚠️ **Correcting a common belief:** MOBI/AZW/AZW3 **are not like EPUB at the container layer**. EPUB is ZIP; MOBI/AZW3 is **PalmDB (PDB)** with a `BOOKMOBI` header, body compressed with **PalmDOC (LZ77 + Huffman)**. What is "EPUB-like" is that **the payload inside is HTML** — so our `lol-html` stage can be reused, while the container-reading stage must be written separately. PalmDOC decompression is only ~150 lines, worth doing.
> ⚠️ `.azw4` **is not** a text book — it is a PDF wrapped in an Amazon container. Do not bother indexing it.
> ⚠️ the `.azw5/6/7/8` numbering is inconsistent in practice; **do not distinguish by number, sniff for `BOOKMOBI`**. Any `.azw3` file without `BOOKMOBI` is treated as not indexable.

**Verified on a real library** (`cargo run --release --bin inspect -- <file.epub>`):
1 EPUB KonoSuba → `detected: Epub`, **25 sections** following the spine, complete metadata
(title/authors/language/series/series_index/identifiers), **405 KB of text** from a 2.2 MB file.

#### 3.0.2 Rules once sniffing is done
- The sniffed format is in the supported list → index.
- The sniffed format is **not** supported (PDF/CBZ/DJVU/KFX…) → write `format` into `BOOKS`, **do not create postings**. If `meta_only_unparseable = 1`, still extract metadata when it can be read (PDF title, file name) so the user can find the book.
- Sniffing **fails** (no magic match, not UTF-8/text) → skip, log at `debug` level. **Do not crash.**
- The sniffed format **differs** from the extension → **trust the magic**, log a `warn` once per book (helps the user spot a messy library).

### 3.1 Scan (metadata only, does not open content)

```
1. walk source_dirs, skipping per §3.5.2 (excluded directories)
2. for each entry:
     ├── a directory named *.sdr or matched by exclude_dir_globs → skip the whole directory
     ├── a hidden file (starts with '.')                          → skip
     ├── extension in exclude_extensions                          → skip (IBM AppleDouble, thumbnails...)
     ├── extension not in include_extensions                      → skip
     └── size outside [min_file_size, max_file_size]              → skip
3. sort by mtime DESC (newest books first — §1.2)
4. for each remaining file:
     look up PATHS[path]
     ├── absent                                    → NEW   : assign id = hwm++, write PATHS
     ├── present, and (size, mtime) matches BOOKS   → SKIP  : do not open the file (0 I/O)
     └── present, but (size, mtime) differs         → UPDATE: assign a new id, the old one is orphaned
5. an id still in BOOKS but no longer on disk       → ORPHAN: delete BOOKS + PATHS + METADATA, postings expire on their own
6. write scan_cursor after every ~200 files
```

**Step 2 does not open files** — only `read_dir` + `stat`. That is the entire reason `include_extensions` exists: without it, we would have to open 64 bytes of **every** file under `/documents` (including images, `.lua`, `.log`) just to know which ones are indexable.

Write `scan_cursor`, and put each book in **its own separate transaction** → a power failure loses at most one book.

> **Do not** use directory mtime to skip a scan. On vfat, a copy made over USB may preserve mtime+size; only `read_dir` (cheap) can detect that a set of files has changed.

#### 3.1.1 Two scan modes: `scope` and `full`

`config_hash` calls `source_dirs` (multiple directories) used only for the **full scan**. But users rarely
wait for indexing to finish before searching — they open the folder they are browsing and start typing right away. Two modes:

| | `scan_scope(dir)` | `scan_full(config)` |
|---|---|---|
| Roots | **one** folder the user currently has open | every `source_dirs` |
| When | the user types a search for the first time | the user presses "index everything", or a periodic schedule |
| Duration | **a few seconds** (a few dozen books) | **a few minutes** (the whole library) |
| `ScanMode` | `Scope` | `Full` |

Rust functions: `scan_scope(engine, filter, dir)` and `scan_full(engine, config)`, both going through
`scan_with_mode(engine, filter, roots, mode)`.

**Search must be scoped as well.** Same reason: the user is in a subfolder and suddenly the query runs over
the entire DB, so (a) results are mixed with books they do not care about, and (b) the first time the DB is empty anyway so there is
nothing to find. The query takes an additional `ScopeFilter::new(dir)`; results keep only books whose path lies under `dir`.

**Important invariant — a scope must not delete books belonging to another scope.** Each book additionally
stores `scope` = the root that discovered it. In `scan_with_mode`, a book is only reported `missing` if
it lies under a root **actually read during this run** (`covered_prefixes`). Without
this guard, the following scenario loses books:

1. The user scope-indexes `/documents/Books/A` (not part of `source_dirs`) → 2 books enter the DB.
2. The user runs a full scan with `source_dirs = [/documents/Books/B]`.
3. Without the guard → the 2 books in A are considered "missing from disk" → **all deleted**.

The same mechanism also protects the case of pulling the SD card: a root that cannot be read ⇒ nothing can be concluded ⇒
no book becomes an orphan.

> Compare using `path == scope || path.starts_with(scope + "/")`, never comparing raw
> bytes, so that `/docs` does not accidentally match `/documents-extra`.

> `scanned_roots` in `ScanPlan` records every root that was traversed (readable or not) so the UI can
> display "which folders have been indexed" and to compute `Coverage` (`Empty`/`Partial`/`Full`).

### 3.2 Extraction, Metadata & Tokenize

Mandatory order: **decode format → extract metadata → tokenize content**. Metadata is extracted once, written to `METADATA`, then fed into the `METAIDX` blob.

1. **Decode the container** according to the sniffed format (§3.0.1) → an ordered list of "sections" with a well-defined reading order:
   - EPUB: entries in OPF `<spine>` order (parse `META-INF/container.xml` → OPF → `<spine>`/`<manifest>`). Fallback if parsing fails: sort by entry name — and set `flags.partial`.
   - MOBI/AZW3: decompress all text records into **a single** section, split by chapter markers if present.
   - TXT/MD: **one** section = the whole file (decode the encoding with `encoding_rs`, preferring BOM → heuristic).
   - HTML/XHTML: one section.
   - FB2: `<body>` → each `<section>` is one section.
2. **Extract metadata** with a dedicated parser for each format (OPF XML / PDB EXTH / FB2 XML / `<title>` / file name). Write `METADATA[book_id]`. If nothing can be extracted → `provenance = filename`, use the file name as `title`.
3. **Build the `METAIDX` blob** for this book: normalize (the same normalizer as §3.2 step 4) then concatenate the fields enabled in `index_fields`, separated by `\n`. This blob will be written back into `METAIDX` when the session closes.
4. The entry is `.xhtml`/`.html` → `lol-html` rewriter, **only record text inside `<body>`**, ignore `<head>/<style>/<script>/<meta>/<!-- -->`.
5. **Clean up chunk boundaries:** the `pending` buffer carries half-finished tokens across chunks. Every tag boundary **flushes** `pending` (prevents `foo<em>bar</em>` from sticking together into `foobar`).
6. **Normalize (mandatory order):**
   ```
   bytes -> utf8 (BOM/entities already handled by lol-html) -> NFD -> strip combining marks (U+0300..=U+036F)
        -> lowercase -> light NFKC (½, ², double punctuation)
   ```
   → `"Thám"` and `"tham"` share the same key.
7. **CJK:** there are no spaces → each CJK character is 1 token (unigram) and **add bigrams** for adjacent character pairs, both carrying the position of the first character. "中文测试" → `中文@p, 文测@p+1, 测试@p+2` and `中@p, 文@p+1, 试@p+2, 测@p+3`. The same position counter set as for Latin words.
8. **Position** = an increasing counter across the whole book (continuous across sections in reading order), starting at 0. **Metadata is NOT in the position space** — it lives in `METAIDX` (§2.3), so position 0 is always the first token of content. This keeps checkpoint/snippet logic clean and free of special cases.
9. **Checkpoint:** every `CHECKPOINT_INTERVAL = 2048` tokens, record the pair `(pos, current byte_off)` → `ENTRYMAP.checkpoints`. This is a **replacement** for storing `byte_off` in each posting.
10. **Soft cap:** at most 100,000 positions for each (book, term); if exceeded, set `flags.partial` + `flags.truncated` in `DICT`.
11. **Safety cap** (per format): section ≤ 32MB decompressed, ≤ 2000 sections, total ≤ 512MB.

### 3.3 Compression & writing

- Sort terms in byte order before writing → linear insertion into redb's B-tree (much faster than random).
- Postings now have only 3 fields: `doc_gap`, `pos_gap`, `entry_idx`.
- Append a new chunk with `chunk_idx = chunk_count`; update `DICT_DELTA` (df, tf, chunk_count).
- Merge `DICT_DELTA` → `DICT` when the session closes.

### 3.4 Max compression strategy

Compression in 6 layers, ranked by **how much is actually saved**, not by how fancy they are:

#### L1 — Drop the most expensive field: `byte_off` *(saves ~15–20%)*
`byte_off` is an absolute value that is nearly uniformly distributed within `[0, entry_len]` → the highest entropy of anything in a posting. V3 **drops it entirely** from postings, keeping only `entry_idx` + `pos`. Snippet extraction later scans for the nearest checkpoint, seeks to that checkpoint's `byte_off`, then re-tokenizes at most 2048 tokens (~10–20KB) to find the exact position. The snippet cost rises by a few ms — and that only happens for ≤3 snippets per book.

#### L1.1 The DB does not duplicate text — it stores a single derived form

A common question: *"does the DB store two copies, one real and one with diacritics stripped?"* **No.**

| | Stored in the DB? | Note |
|---|---|---|
| The book's actual text | **No** | It lives in the `.epub` on disk, which remains the single source. The whole library measures 348,7 MB — it is not duplicated into the DB |
| Folded term + position + doc_id | **Yes** (`POSTINGS`) | This is **not** a "diacritics-stripped copy of the text" but a different structure: isolated terms with their positions. The whole library measures 102,9 MB |
| Display metadata (title, author…) | **Yes** (`METADATA`) | A few hundred bytes per book |

So the DB takes only ~103 MB for the entire index, not ~452 MB.

#### L1.2 Exact positions look redundant, but the measurements say otherwise

A fair question: *"if previews still have to dig back through the `.epub`, what is the point of storing detailed
positions?"* — because digging back through it **is not a scan, it is a seek to an address we already know**.

Measured on 120 real books (12,7M postings), `bin/postest`:

| Storage scheme | Size | Whole library | Phrase query "tham tu" |
|---|---|---|---|
| doc_id + freq only | 2,0 MB | ~10 MB | every candidate book must be extracted ≈ **9 seconds** |
| Positions blurred to 1024 tokens | 6,1 MB | ~31 MB | too many false positives |
| Positions blurred to 256 tokens | 8,4 MB | ~43 MB | too many false positives |
| Positions blurred to 64 tokens | 11,3 MB | ~58 MB | 5903 candidates must be verified (**5,6 false / 1 correct**) |
| **Exact positions** | 20,7 MB | **102,9 MB** | **896 correct, 0 false positives**, ~5ms |

The "blur to 64 tokens" row looks like a bargain (−45% footprint) but produces 6,6× as many candidates
to verify, each ~10ms of extraction → queries get ~6 seconds slower. It **self-destructs** against
the p95 < 50ms budget chosen in §1.4. Trading 45MB for 5ms is a price well worth paying.

> **Positions and checkpoints are not the same thing.** A position answers "which token is it
> in the book"; a checkpoint answers "which token sits at which byte in the file". Both are
> needed: without positions you must scan the whole book, and without a checkpoint a position
> cannot be translated into a place to read.

**`byte_off` in a checkpoint must point at the RAW byte of the entry in the ZIP, not into the decoded text.**

Why: the raw bytes of an `.epub` never change; if the file changes, `size`/`mtime` change and we re-index. Conversely, if I change one line of `lol-html` config, the entity decoder, or add a filter — every offset of the "into the decoded text" kind will point at the wrong place **with no error at all**, the snippet simply displays wrong content. That is the hardest class of bug there is to debug.

#### L2 — Choose the coder by the **monotonicity** of each column *(saves ~10–20% versus PForDelta)*
This is the crux of the matter: each column has a different shape, and using one coder for all 4 columns is a waste.

| Column | Shape | Coder | Why |
|---|---|---|---|
| `doc_gap` | monotone, small gaps | **Elias-Fano** | near-optimal for monotone sequences, overhead only ~2 bit/element, decode drains the stream |
| `pos_gap` | monotone within a doc, amplitude depends on the doc | **Binary Interpolative Coding (BIC)** | optimal when the upper bound is known; clearly beats PForDelta on large and varying gaps (matching the tail of the Zipf distribution) |
| `entry_idx` | nearly constant throughout a doc | **RLE** | becomes runs `(value, run_len)` → ~0.1 bit/posting |
| ~~`byte_off`~~ | — | **dropped** (L1) | — |

Block = 128 postings. Each block picks its coder based on the actual `frac_bits` of its range; if a value overflows it goes into the exception list. If BIC decode turns out too complex for the deadline, the fallback is **OptPFD** (choosing between PForDelta and VByte per block) — a few percent worse than BIC at compression, but far simpler.

#### L3 — An entropy layer on top *(saves ~10–15%)*
After L2, apply `zstd` over the whole encoded block. Already entropy-coded data still compresses further because PForDelta/Elias-Fano leave a lot of redundant bits. Use a **trained dictionary** (trained on ~500 sample chunks) — our chunks are small and uniform in shape, so a trained dictionary typically gains another 5–10% over untrained zstd.

**Level: `zstd` level 3 — do NOT use level 19.** Measured on 348,7MB of real text:

| level | size | time (x86 host) |
|---|---|---|
| 3 | 100,9 MB (28,9% of text) | 6,5 s |
| 19 | 68,4 MB (19,6% of text) | **512 s** |

Level 19 is only **32%** smaller than level 3 but **~79×** slower. On a Cortex-A9 (roughly 10–20× slower than the host) level 19 takes **1,5–3 hours** for one full-library build, while level 3 takes only **~2 minutes**. Level 19 is only usable when building offline on the host and then copying to the Kindle — declare that path as an option, **not the default**.

Decompression is fast at every level (~1GB/s) and we only decode ~10KB per queried chunk, so the level choice only affects the *write* path, not the p95 < 50ms budget.

#### L4 — A compact dictionary *(saves ~30–50MB + faster)*
Front coding on the sorted dictionary: store `[prefix_len][suffix]` instead of the whole term → cuts term text size by ~50–60%. Group into blocks of 32 terms so binary search still works. Then zstd. And put it in **a single KV** (`DICT`) — see §2 for why 1M KVs would cost 30–50MB in redb page overhead alone. As a bonus, prefix search goes from "a few page reads" to "a binary search in RAM".

#### L5 — The truth about redb (docs checked, don't guess)
**redb 2.x has no native compression.** `Database::builder()` only provides `set_cache_size()`, `set_page_size()`, `set_repair_callback()`, `set_region_size()`. A value going into redb is raw bytes, passing through no compression layer whatsoever.

→ Consequence: **every layer from L1 to L4 must be implemented by us before writing**, and if someone drops a layer the index will be 2–5 times larger with no warning at all from redb. This is also why our schema stores `codec_version` in `META`.

#### L6 — Space reclamation
- Orphaned postings (books deleted or changed) remain in the DB. Compact/rebuild everything once orphans exceed 25% of the DB.
- redb is copy-on-write: it rewrites the whole chunk (rather than patching in place) → `fragmented_bytes` will grow over time. Call `compact()` periodically.

#### Budget summary

| Component | Technique | ~bit/posting | 600M postings |
|---|---|---|---|
| `doc_gap` | Elias-Fano | 2.5 | ~190 MB |
| `pos_gap` | BIC | 5.5 | ~415 MB |
| `entry_idx` | RLE | 0.3 | ~23 MB |
| `byte_off` | **dropped** → `ENTRYMAP` | 0 | ~5 MB |
| block header + exceptions | | 1.0 | ~75 MB |
| zstd-19 + trained dict | −15% | | −105 MB |
| **Postings** | | **~8.6** | **~600 MB** |
| Dictionary | front coding + zstd, 1 KV | | ~5 MB |
| `BOOKS`/`PATHS`/`ENTRYMAP`/`DICT` | | | ~20 MB |
| `METADATA` + `METAIDX` (6.000 books) | compressed metadata blob ~2MB | | ~15 MB |
| **TOTAL (planning estimate)** | | | **~640 MB** |

> ⚠️ The numbers in the table are **estimates** for choosing coders, not commitments. The error can be ±50% (a corpus with few repeated words will do better, a corpus heavy in proper names/code will do worse), which is why §1.1 records **roughly 500–700MB** instead of a single figure. The point that must be decided is §7.7 — measure for real on 300 books and then tune. If the measurement comes out much worse, the first thing to tune is `pos_gap` (which accounts for 2/3 of the budget), followed by enabling `index_depth = balanced`.

---

### 3.5 File extension and directory configuration

The user configures this from the KOReader UI → FFI `config_set()` → the `CONFIG` table. There is no separate configuration file, and no paths are hardcoded in Rust.

#### 3.5.1 Default file extension list

**Indexed by default** (they contain text to read):
```
epub  kepub  mobi  azw  azw3  txt  text  md  markdown
fb2  fb2.zip  html  htm  xhtml  xht  htm  shtml
```

**Excluded by default** (junk, not books):
```
lua  log  json  xml  opf  ncx  css  js  mjs  py  rs  sh  bash  toml  yaml  yml
ini  cfg  conf  bak  tmp  temp  part  partial  swp  old  orig  db  db3  sqlite3
sqlite  png  jpg  jpeg  gif  webp  bmp  ico  svg  mp3  mp4  mkv  woff  woff2  ttf
zip  gz  tgz  bz2  xz  7z  rar  tar  zst
pdf  djvu  djv  cbz  cbr  cb7  cbt  azw4  azw6  azw8  kfx  tpz
sdr  opf  toc  ncx  calibre  cover
```
> Note: `pdf/djvu/cbz` are in the exclusion list **as far as content search** is concerned (§3.0.2), but they are still excluded at the pre-filter step. If `meta_only_unparseable = 1` is enabled, they are **added to** `include_extensions` so their metadata is still indexed.

**Conflict resolution rule:** `exclude_extensions` **wins over** `include_extensions`. If the user adds `.azw3` to include while `.azw4` is in exclude, `.azw4` is still dropped.

**If the user wants to add a new extension:** add it to `include_extensions` (e.g. `.azw4`, `.dat`, `.prc`). If the file is in fact MOBI/EPUB/TXT, magic sniffing (§3.0) still recognizes it correctly. This is the strength of keeping pre-filter and sniff separate.

#### 3.5.2 Directory exclusion

```
exclude_dir_globs (default): *.sdr, .calibre*, .thumbnails, @eaDir, __MACOSX,
                              koreader, system, extensions, .Trash*, lost+found
```
A directory matching the glob → **skip the entire subtree** (do not recurse into it). This is an important optimization: KOReader's `<book>.sdr/` contains `metadata.epub.lua`, `custom.css`, `cover.jpg` — all junk as far as we are concerned.

Additionally, ignore every entry starting with `.` (unless the user adds the glob `.*` to include).

Default scan source: `/mnt/us/documents`. The user can add `/mnt/us/books` or any other directory.

#### 3.5.3 Which options require an index rebuild, and which only need a rescan

This is the easiest place to cause a silent bug — **changing a sensitive option without rebuilding = a wrong index, and nobody knows.**

| Option | Impact | Action when changed |
|---|---|---|
| `fold_diacritics` | changes the token stream | **full rebuild** (bump `config_hash`) |
| `cjk_bigrams` | changes the token stream | **rebuild** |
| `index_fields` | changes the metadata blob contents | **rebuild `METAIDX`** |
| `include_description` | changes the metadata blob contents | **rebuild `METAIDX`** |
| `index_depth` (§1.2) | changes positions | **rebuild** |
| `checkpoint_interval` | does not change tokens, only checkpoint density | rewrite ENTRYMAP at the next index pass |
| `include_extensions` | changes the set of files scanned | **rescan** (no rebuild) |
| `exclude_extensions` | changes the set of files scanned | **rescan**; dropped files become self-cleaning orphans |
| `exclude_dir_globs` | changes the set of files scanned | **rescan** |
| `source_dirs` | changes the set of files scanned | **rescan** |
| `min/max_file_size` | changes the set of files scanned | **rescan** |
| `meta_only_unparseable` | changes the set of files scanned | **rescan** |
| `prefix_search_mode` | only changes how a query expands terms, does not change the token stream | **no rescan, no rebuild** needed — just reload the engine |

The group above only needs a rescan because the postings of old books **do not change at all** — that is a direct consequence of the "re-index = issue a new `book_id`" design (§0 #5).

#### 3.5.4 The best way to avoid unnecessary rebuilds
The `config_hash` in `META` is computed from the **rebuild group** in the table above only. The rescan group can change freely. If someone happens to add a sensitive option and forget to include it in the hash → this section is the place that must be updated, and §7 has a regression test for exactly this.

---

## 4. Query

### 4.1 Phrase query (book content)
1. Tokenize the query using the **exact same** normalizer as in §3.2 (ensuring the prefix property) → if CJK is present, additionally generate bigrams.
2. Look up `DICT` (+ `DICT_DELTA` if present) → `df` for each term. **Sort by ascending `df`** (rarest first).
3. For the rarest term: iterate chunks in order; within each chunk decode each block (only ~64KB RAM/decode), perform a **positional intersect** with the constraint `pos[i+1] - pos[i] = gap_i` (`gap = 1` for adjacency — correct because stopwords are still indexed).
4. Binary-search the chunks for the remaining terms using `first_book_id` at the start of the value, instead of decoding everything.
5. A posting pointing to a `book_id` that is no longer in `BOOKS` (orphan) → skip it.

#### 4.1.1 Fallback prefix expansion — **mandatory, not optional**

**Problem:** the query `cha` only matches the word `cha`. In a real library, users type "cha", "sher", "nguy" intending "chai", "sherlock", "nguyễn". On the PW3 keyboard (d-pad, slow typing, easily tiring) **typing a fragment of a word is the dominant behavior**, not an edge case. An exact-only mode would make search fail frequently.

**Solution — the rule "if it is not in the dict, expand it into a prefix":**

```
for each query term t:
    if t ∈ DICT                     → term_set(t) = { t }              # a complete word, do NOT expand
    if t ∉ DICT                     → term_set(t) = prefix_scan(t)     # expand
```

Why expand only when t is **not** in the dict: if `t` is a word that really exists (e.g. `cha`, `the`, `nó`) then the user intends to type that exact word — expanding would make results match at random. If `t` is not any word in the entire library (`sher`, `nguy`) then it is almost certainly a word that has not been finished being typed. This rule delivers high accuracy at nearly zero cost.

**Algorithm details:**
1. `prefix_scan(t)` = range scan of `DICT` over `[t, successor(t))`, fetched in descending `df`, capped by both limits: **≤ 64 terms** and **total `df` ≤ 100,000**. Exceeding either → return empty, set `status = too_broad` (tell the UI "too many results, type more characters").
2. For each term t, **merge the postings of the entire `term_set(t)` into a single stream** (`union_postings`). The stream still preserves `(book_id, pos)` order — see the note below.
3. The positional intersect stays the same: `pos[i+1] - pos[i] = gap_i`. So the query `cha chai` with `cha` → `{chai, chan}` will correctly match both "chai chai" and "chan chai". **There is no need to pair terms one by one (it is not |T1|×|T2|).
4. If `term_set(t)` has > 1 term → mark the match as `fuzzy`. **This must be shown to the user** (e.g. underline the expanded term in the search box), otherwise they will not understand why typing `cha` returns books that do not contain the text "cha".
5. Ranking: a match on an expanded term is downweighted by a factor of `0.7` compared to exact.

**User setting:** `prefix_search_mode` = `auto` (default, the rule above) | `exact` (match full terms only) | `prefix` (always expand every term, including words already in the dict — for users who want maximum recall).

**Why `union_postings` is still ordered:** in `DICT` the terms are already sorted by byte, so terms that begin with the same prefix are **adjacent in redb's B-tree**. `union_postings` merges the chunks in `(term, chunk_idx)` order → within the scope of a single term the sort is still correct by `(book_id, pos)`. The two-pointer intersect still runs linearly.

### 4.2 Metadata search (title / author / series / tag)
Runs **in parallel and independently** of §4.1, without using postings:

1. Load `METAIDX` (2–3MB compressed → ~5MB RAM, LRU 1, loaded only once per query batch).
2. Normalize the query using the exact normalizer from §3.2 → `q`.
3. For **every** book blob (6000 blobs × ~300 bytes = 1.8MB): `memmem(blob, q)`. SIMD memchr scans 1.8MB ≈ < 3 ms on Cortex-A9.
4. If `q` matches no blob but matches in token form → also try matching each token of `q` (so that "nguyễn nhật ánh" finds "Nguyễn Nhật Ánh" when the blob contains that exact string).
5. Return `(book_id, field_mask)` indicating which field matched (`title`, `authors`, `series`, `tags`…).

**Why an inverted index is not used for metadata:** postings can only match a **whole token** or a prefix, whereas a user typing `nguy` certainly wants `Nguyễn`, and typing `imov` wants `Asimov`. Substring matching over 1.8MB is both fast enough and sufficient — no index needed.

### 4.3 Prefix / suggestions (autocomplete)
- For **words in content**: binary search in `DICT` (already loaded in RAM, §3.4 L4) from the prefix up to its successor (e.g. `"sher"` → `"shes"`). Limit to the first 200 terms, merge postings.
- For **metadata**: scan the 6000 blobs in `METAIDX` with the prefix of each field. Same cost, ~3 ms.
- Merge both suggestion sources, prioritizing metadata hits (book title/author) first.
- **Reuse the exact `prefix_scan()` function from §4.1.1** — do not write it twice. Suggestions are a preview of the user pressing Search, so the two steps must produce consistent results.

### 4.4 Ranking (merging 2 sources)
```
score(book) = w_meta × meta_hits × idf_meta     # title/author match — the strongest signal
            + Σ_term  tf_term_in_book × idf_term  # content match
```
`w_meta` defaults to high (~3×) because when the user types a book title or author name the intent is almost certainly to find that specific book, even if the content may also contain that phrase. Sort descending, group by book, each book keeps at most 3 results.

### 4.5 Snippet & opening a book
1. **Content match:** `ENTRYMAP[(book_id, entry_idx)]` → `entry_name` + `checkpoints`.
   - Find the nearest checkpoint **≤** `pos` (binary search in the decoded array).
   - Open the file, seek to the checkpoint's `byte_off`, continue decoding with `lol-html`, count tokens up to `pos` (≤ 2048 tokens), take ~700 bytes around the match → highlight.
   - LRU cache of the 2 most recent files (to avoid reopening the same book as the user scrolls through results).
2. **Metadata match:** there is no text snippet — return a **metadata card** (title, authors, series, publisher, year, tags) from the `METADATA` table. This is what the user expects when searching by book title.
3. **Lazy** when the user *taps* a result: count elements up to that offset → build a `Locator` (xpointer) so KOReader jumps to the right page. A few ms, once per selected result.

---

## 5. KOReader integration

| Item | Decision |
|---|---|
| Artifact | `crate-type = ["cdylib"]` → `libturbosearch.so`, exporting only `extern "C"` functions + an opaque handle (`Box::into_raw`) |
| Cross-compile | target `armv7-unknown-linux-gnueabihf`, **same toolchain/glibc as koreader-base** (hard-float, NEON). Do not hard-code `-C target-cpu=cortex-a9` so it can still run on other devices |
| Lua side | `ffi.cdef` + `ffi.load("/mnt/us/koreader/search/libturbosearch.so")` |
| Threading | Indexing runs in KOReader's background task; the FFI exposes `progress()`. **Never block the UI thread** |
| DB location | `/mnt/us/koreader/search/turbosearch.redb` |
| Crash safety | redb has WAL/ACID → a power loss will not corrupt the DB, losing at most 1 book (§3.1) |
| ABI check | the `so` checks `schema_version` at load; mismatch → return a "rebuild needed" error, no crash |

Crates:
```
redb 2.x · zip 2.x · flate2 (miniz_oxide — pure Rust, no C zlib needed)
lol-html 1.x · unicode-normalization · rustix (statvfs) · libc

Multi-format & metadata:
  quick-xml         — parse OPF (EPUB), FB2 <description>, META-INF/container.xml. Light, pure Rust.
  encoding_rs       — TXT/MD/HTML: BOM sniff + heuristics for UTF-8/UTF-16/cp1252/Shift-JIS/GBK.
  memchr            — memmem for METAIDX (§4.2), and faster than a naive scan inside the indexer.
  (optional) ebook-rs 0.16.x — only for long-tail formats, feature-gated `--features longtail`.
       Leading options: PDF (pdf_oxide), DOCX, RTF, ODT, LIT, KFX.
       Pure Rust, magic-byte detection, exposes metadata() + sections(). But still at 0.16,
       with heavy dependencies (zlib-rs, regex, whatlang, simdutf8, serde_json...) and 45% doc coverage.
       → do NOT use it for EPUB/TXT/MOBI/FB2 (4 formats covering >95% of a Kindle library, all
       implementable here).
       Only enable it when the user genuinely has unusual extensions and sets longtail_formats = 1.
```

PalmDOC decompression (~150 lines, hand-written): see §3.0.2. There is no stable crate, and this is the core of MOBI/AZW3, so it must be written by hand.

Written in-repo (no existing crate is good enough): Elias-Fano, BIC, RLE, front coding, PForDelta as a fallback. About ~600 lines total + property test.

`Cargo.toml` profile:
```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true

[features]
default = []
longtail = ["ebook-rs"]     # off by default: smaller binary, fewer dependencies
```

---

## 6. RAM / I/O on PW3

- **`METAIDX` loaded into RAM when there is a metadata query**: ~2–3MB compressed → ~5–6MB decompressed. LRU 1, releasable when idle.
- `DICT` loaded into RAM ~10–15MB resident (LRU 1). The largest RAM item.
- **Both should share a single budget** `query_arena` ~24MB; if the query does not need metadata, `METAIDX` is not loaded.
- Per-book accumulator (hashmap term → Vec<posting>) ~5–15MB for a typical book; outlier books > 200MB of text → **flush per chunk instead of keeping everything**.
- redb `set_cache_size(8..=16 MB)` — **redb's default is 1GiB**, which on a 512MB RAM device is a serious bug if forgotten.
- I/O: the scan phase only `stat`s → cheap. Heavy I/O occurs on the first index pass and for snippets (cold read of 1 ZIP entry).
- Keep the zstd buffer fixed at ~64KB, do not allocate it again on every chunk.
- **Rewriting `METAIDX` once per session** = compressing ~5MB → ~1ms CPU, not worth worrying about. But **do not** rewrite it after every book.

---

## 7. Testing

1. **Property test** each coder: random `Vec<u32>` → encode → decode → equal. Elias-Fano, BIC, RLE, PForDelta, front coding, one million cases each on the host.
2. **Compression benchmark** on the host: measure actual bits/posting for each coder using synthetic Zipf data + one real English/Vietnamese/Chinese corpus → choose the coder based on measurements, not gut feeling.
3. **Golden test** tokenizer: 20 sample HTML/EPUB files (entity, comment, CDATA, `>` in attribute, UTF-16, CJK, Vietnamese) → snapshot term + position.
4. **Compare** with reference `html5ever`/`lol-html` output on the same input.
5. **Round-trip** index → query for Vietnamese phrases with and without diacritics, CJK, phrases of 4–5 words, and phrases containing a stopword.
6. **Resume**: kill in the middle of indexing → run again → DB is valid, no completed book is lost.
7. **Benchmark on a real device** of 300 books: measure p50/p95 query, indexing time, **actual DB size** → adjust the table in §3.4. This is the gate of phase 4.
8. **Format sniff test** (v4): 1 sample file/format × each magic in the table at §3.0.1 → assert the enum is correct. Also test: files renamed with the wrong extension, empty files, 3-byte files, zip bombs, files with matching magic but corrupted partway through.
9. **Metadata test** (v4): each format × metadata in the correct field; books with missing metadata → `provenance = filename`; description > 4KB → `desc_truncated`; Vietnamese/CJK characters in title preserved with diacritics.
10. **Metadata search test** (v4): `nguy` → `Nguyễn`, `imov` → `Asimov`, `nhật ánh` → `Nguyễn Nhật Ánh`, filename `harry potter` → file `Harry.Potter...txt`.
11. **Config test** (v4): change the "rebuild" group → assert `config_hash` changes; change the "rescan" group → assert `config_hash` does **not** change and new books are still indexed. Regression protection for §3.5.3.
12. **Prefix expansion test** (v4): index a corpus containing "chai", "chair", "chaos" but **not** "cha" → assert the query `cha` returns all three, and that the match is marked `fuzzy`. Index a corpus that does contain "cha" → assert the query `cha` does **not** expand (exact-only). Query `cha chai` with `cha`→`{chai,chan}` → assert it matches both "chai chai" and "chan chai" by adjacency. Query with an overly broad prefix (`a`) → assert `status = too_broad` rather than hanging.

---

## 8. Not supported (stated explicitly so users are not confused)

- **KFX, CBZ, DJVU, PDF, DOCX**: not indexed → search results will not include them. No error is reported.
- **MOBI/AZW3**: phase 2 (requires PalmDOC + HTML Huffdic decompression).
- EPUB corrupted / oversized entry: skip that book, log it, **do not crash**.
- Safety caps: each entry ≤ 32MB decompressed, ≤ 2000 entry/book, total ≤ 512MB (zip bomb protection).
- Known and accepted limitation: an entity split across 2 chunks (`&am` + `p;`) may produce 1 incorrect token very rarely.
- Snippet after L1 is a few ms slower (must rescan ≤ 2048 tokens) — already accounted for in the budget at §1.4.

**Not supported (stated explicitly so users are not confused):**
- **PDF / AZW4 (Print Replica) / DJVU / CBZ / CBR**: content is not indexed. The user still sees the books in the KOReader library as normal; it is only full-text search that does not return them. If `meta_only_unparseable` is enabled, they can still be found by filename.
- **KFX (KF10)**: Amazon's proprietary CONT/Ion container — **not indexed in phase 1–2**. Many books purchased on Amazon are KFX, so this is the biggest limitation; it is already recorded in the phase 3 roadmap.
- **AZW5/6/7/8**: named inconsistently in practice. Handle via magic sniff (§3.0.1), do not guess by the number.
- **LIT / RTF / DOCX / ODT**: phase 3, or opt-in via `longtail_formats`.
- **Books with DRM (Kindle/Adobe)**: not indexed. No DRM cracking — we can only read files that have already been decrypted.
- **CBZ/CBR**: images only, no text to index. No OCR (too heavy for the PW3).
- **KOReader metadata sidecar** (`metadata.epub.lua`): phase 3, see §2.3.
- **Text inside images / scans**: not indexed (no OCR).

---

## 9. Roadmap

| Phase | Content | Definition of done | Status |
|---|---|---|---|
| 1 | Cargo + module layout + 4 coders (Ef/BIC/RLE/PForDelta) + property test | Round-trip passes for each coder, bits/posting benchmark | ✅ **done** — 52 tests, 0 warnings |
| 1b | Real-corpus measurement (`bin/measure`) | Whole-library measurements, no extrapolation | ✅ **done** — 614 EPUBs: 1.36GB→348.7MB text→full index 102.9MB |
| 2 | redb schema + `pack` + SCAN + BOOKS/PATHS/META/CONFIG + allocate id | Detect new/update/delete correctly; ids not reused; resume cursor | ✅ **done** — 48 integration tests, 3 real bugs caught |
| 3 | **Format sniff + TXT/HTML/EPUB decoders** (magic byte + OPF spine + lol-html) | Golden test §7.3 + §7.8 pass; run on the real library | ✅ **done** — 127 tests; 25 sections/books, 405KB text from 1 EPUB |
| 4 | **Metadata: OPF + FB2 + filename + METAIDX blob + provenance** | §7.9 + §7.10 pass — books findable by title/author | ✅ **done** — 9 integration tests; **43/43** books have a sidecar |
| 5 | POSTINGS + DICT/DICT_DELTA + chunk append | Check actual DB size across the whole library vs. the table at §1.1.1 | ⬜ |
| 6 | **MOBI/AZW/AZW3 decoders** (PalmDB + PalmDOC + EXTH) | Correctly decode text + metadata of 5 sample files in the library | ⬜ |
| 7 | Query: phrase + prefix + rank + snippet + metadata match | p95 < 50ms on a real device | ⬜ |
| 8 | FFI + build armhf + LuaJIT integration | Runs on a real Kindle PW3 | ⬜ |
| 9 | Budget guard, compact, progress UI | Release | ⬜ |
| 10 | Long-tail: KFX, KOReader sidecar, `longtail` feature | Optional | ⬜ |

---

---
# APPENDIX — SPEC v1 (original, kept for history)

> The content below is the `note.md` from before the upgrade. **It is no longer the current guide** — the defects/changes are recorded in §0.

```
check file change = inode + mtime + size
check dir change = mtime
```

## TECHNICAL SPEC: EMBEDDED FULL-TEXT SEARCH ENGINE IN RUST FOR KOREADER

### 1. Environment & Hardware Constraints

* Target device: Kindle Paperwhite 3 (PW3) – single-core ARM CPU, limited RAM (512MB - 1GB), slow disk read/write (I/O).
* Language: Rust (cross-compiled to the ARM architecture).
* Database: redb (embedded key-value store, pure Rust, single-file format).
* Performance goal: real-time phrase query (< 50ms). DB footprint under 3% of the library size (~50MB estimated for 2.5GB of books).
* Strategy: no background file-monitoring daemon (watch fs); only manual scanning on demand (on-demand incremental scan). Accept a few false positives from raw HTML source in exchange for blazing speed.

### 2. Database Schema (redb)

**Table A: BOOKS_TABLE (Book Catalog Management)**
* Key: book_id: u16 (unique identifier, saves space, up to 65,535 books).
* Value: packed byte structure containing: [path: String] + [inode: u64] + [mtime: i64] + [size: u64].
* Purpose: ultra-fast file change detection using system metadata without having to read file contents.

**Table B: INVERTED_INDEX_TABLE (Inverted Index)**
* Key: word: String (lowercase keyword).
* Value: compressed encoded byte array: CompressedPostingsList.
* Data structure after decompression (logical): Vec<(book_id: u16, positions: Vec<u32>)>.

### 3. Data Processing Pipeline

#### Step 1: Super-Fast Incremental Scan
1. Read the mtime of the main books directory. If it matches the previously stored value, skip the entire file scan.
2. If the directory has changed, iterate over the files (std::fs::read_dir) and quickly fetch the metadata triple (inode, mtime, size).
3. Compare against BOOKS_TABLE: match → skip the file; changed/new → needs indexing; missing from disk → delete the old data.

#### Step 2: Rough Tokenizer & Position Extraction (Positioning)
1. Use direct streaming from memory (memory streaming) via the zip crate, without writing files to flash.
2. Use a minimal state machine: `InTag` (skip from `<` to `>`), `InText` (split words, lowercase).
3. Assign an increasing counter index (0, 1, 2, 3...) to each valid word. Filter out stopwords.

#### Step 3: Position Data Compression (Delta Encoding + VByte)
1. Delta Encoding: [100, 105, 120] → [100, 5, 15].
2. Variable Byte (VByte / Varint): numbers < 128 cost only 1 byte.

### 4. Phrase Query Algorithm
1. String decomposition: split the search sentence into individual tokens.
2. In-RAM query: fetch the postings list of 2 words, decompress Delta + VByte.
3. Positional intersect two-pointer O(N+M): condition `Position(từ_2) - Position(từ_1) = 1`.
4. Displaying results: return the book_id and the byte/word position, `seek()` to extract a snippet.

### 5. Recommended Libraries (Crates)
* redb, zip, bstr or regex

### 3.1 Supplement: Clean Text Extraction (HTML Parsing Strategy)
* Recommended library: `lol-html` (streaming tokenizer form; absolutely do not use DOM-building libraries such as `scraper`).
1. Only extract the text content (Text content) inside the `<body>` tag.
2. Completely ignore (Ignore) all tags and the child content inside the tags: `<head>`, `<style>`, `<script>`, `<meta>`.
3. Automatically decode HTML entities (for example: convert `&nbsp;` into a space, `&amp;` into `&`) before feeding them into the position counter (Tokenizer).
4. After extracting the clean text stream, then assign increasing position indices (0, 1, 2, 3...) to each word.
