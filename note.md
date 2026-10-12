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

# TURBOSEARCH — ĐẶC TẢ KỸ THUẬT v4

**Full-Text Search Engine (phrase + prefix) cho KOReader trên Kindle Paperwhite 3, viết bằng Rust.**

> Tài liệu này thay thế hoàn toàn bản v1 ở Phụ lục cuối file.
> v2 thêm: quyết định Q1 (phương án a), lol-html, spine order, không tái sử dụng book_id.
> v3 thêm: **quy ước English-only cho code** (§1.3) và **chiến lược nén tối đa** (§3.4).
> v4 thêm: **index metadata sách** (§2.3, §3.2) và **đa định dạng + cấu hình đuôi file** (§3.0, §3.5).
> Các thay đổi được liệt kê ở §0.

---

## 0. Nhật ký thay đổi

### v1 → v2

| # | v1 | v2 | Lý do |
|---|---|---|---|
| 1 | DB < 3% thư viện (~50MB / 2.5GB) | **Chấp nhận ~20–25%** + ngân sách đĩa (§1.2) | Không thể đạt 3% với positional index — §1.1 |
| 2 | State machine tự viết (InTag/InText) | **`lol-html` duy nhất** | `>` trong attribute, `<!-- -->`, CDATA, entity, UTF-16 làm lệch position và đưa rác vào index |
| 3 | Đọc entry ZIP theo thứ tự `read_dir` | Đọc theo **OPF `<spine>`** | Position phải khớp thứ tự đọc, nếu không snippet hiện sai chỗ |
| 4 | Lọc stopwords | **Index toàn bộ từ**; cap mềm 100K pos/(sách, từ) | Đã chấp nhận size lớn → adjacency chính xác (= 1), cụm "the girl with…" khớp đúng |
| 5 | Xóa sách = dọn posting cũ | **Không tái sử dụng `book_id`**; posting mồ côi discard khi query; compact định kỳ | Xóa 1 sách không phải quét 600M posting → O(1) |
| 6 | Delta + VByte | PForDelta (sau đó nâng lên §3.4 v3) | Nén tốt hơn, decode nhanh hơn ~2–4× trên Cortex-A9 |
| 7 | `inode + mtime + size`, check dir bằng mtime | `path + size + mtime`; `inode` chỉ là hint; **bỏ** check dir-mtime | `/mnt/us` trên Kindle là **vfat**: inode không ổn định, mtime chỉ có độ phân giải **2 giây** |
| 8 | Trả về "vị trí byte/từ", `seek()` vào file thô | `entry_idx + byte_off` + `ANCHORS`; xpointer **lazy**; snippet decode bằng `lol-html` | KOReader mở sách bằng `Locator`; `seek()` vào HTML thô sẽ lộ tag |
| 9 | Tokenizer tách theo khoảng trắng | **Fold diacritics (NFD → strip combining marks)** + **bigram cho CJK** | "tham tu" phải ra "thám tử"; tiếng Trung/Nhật không có space |
| 10 | Không có | **Prefix search** (range-scan dictionary) + **ranking tf×idf** | UX bàn phím e-ink; thứ tự `book_id` là vô nghĩa |
| 11 | Không đề cập | **Resume**: cursor + 1 transaction/sách | PW3 pin yếu, index lần đầu 10–30 phút |
| 12 | Chỉ nói EPUB | Scope rõ: **EPUB/TXT/FB2** phase 1, **MOBI/AZW3** phase 2, **KFX/CBZ/DJVU/PDF/DOCX không index** | Thư viện Kindle có nhiều AZW3 |
| 13 | Không có | `META` chứa schema/tokenizer/normalizer version + `config_hash` | Đổi 1 dòng normalize là toàn bộ index cũ thành rác vĩnh viễn |
| 14 | Không đề cập | `cdylib` + C ABI, load bằng LuaJIT FFI, cross-compile `armhf` (§5) | KOReader là LuaJIT + C |
| 15 | Không đề cập | Cap entry / tổng size giải nén / kiểm tra free space | EPUB là input không tin cậy (zip bomb); đầy đĩa làm hỏng máy |

### v2 → v3 (bản cập nhật này)

| # | v2 | v3 | Lý do / Tiết kiệm |
|---|---|---|---|
| 16 | Không quy định | **English-only cho toàn bộ code, comment, log, error, test name, commit** (§1.3) | Lỗi hiện ra log của KOReader phải grep được; rustc/clippy/rustdoc đều English; sẵn sàng cho contributor ngoài |
| 17 | "redb nén được" (ngầm định) | **redb 2.x KHÔNG có nén native** — đã kiểm tra tài liệu: `Builder` chỉ có `set_cache_size`, `set_page_size`, `set_repair_callback`, `set_region_size`. Toàn bộ nén phải tự làm (§3.4 L5) | Né bẫy: đợi redb nén giúp = index toàn bộ không nén |
| 18 | Posting chứa `byte_off: u32` | **Bỏ `byte_off` khỏi posting**; thay bằng `ENTRYMAP.checkpoints` — mảng `(pos, byte_off)` thưa mỗi 2048 position (§3.4 L1) | **~15–20%** — đây là field có entropy cao nhất trong posting |
| 19 | `TERMS` = 1 KV cho mỗi term (~1M KV) | **`DICT` = 1 KV duy nhất**, front-coded + zstd, chứa cả df/tf/chunk_count; `DICT_DELTA` cho session đang chạy (§3.4 L4) | **~30–50MB** overhead B-tree/page + prefix search thành binary search trong RAM |
| 20 | PForDelta 4 cột | **Elias-Fano** (doc gap) + **Binary Interpolative Coding** (pos gap) + **RLE** (entry_idx), rồi **zstd-19 + trained dictionary** lên trên (§3.4 L2–L3) | **~10–20%** so với PForDelta |

### v3 → v4 (bản cập nhật này)

| # | v3 | v4 | Lý do |
|---|---|---|---|
| 21 | Chỉ index nội dung sách | **Index cả metadata sách** (title, author, series, tags, publisher, description…) qua bảng `METADATA` + `METAIDX` (§2.3, §3.2) | Người dùng tìm sách theo tên/tác giả/thể loại, không chỉ theo nội dung |
| 22 | Chỉ hỗ trợ EPUB (TXT/FB2/AZW3 để phase 2) | **Đa định dạng ngay phase 1**: EPUB/KEPUB, MOBI/AZW/AZW3, TXT/MD, HTML/XHTML, FB2 (+`.fb2.zip`) | Thư viện Kindle thật chứa nhiều `.azw3`/`.mobi`; bỏ qua = bỏ qua phần lớn sách |
| 23 | Không có | **Cấu hình được**: `include_extensions` / `exclude_extensions` / `exclude_dir_globs` / `source_dirs` (§3.5) | Người dùng tự thêm đuôi muốn quét, tự loại trừ đuôi rác |
| 24 | Không có | **Nhận diện format bằng magic byte**, extension chỉ là pre-filter nhanh (§3.0) | `.az`/`.azw`/`.azw3`/`.kfx` đặt tên lộn xộn trong thực tế; file đổi tên cũng phải index đúng |
| 25 | Snippet chỉ trích text sách | Match trong metadata → trả **thẻ metadata** (title/author/series) thay vì snippet | Kết quả tìm theo tên sách phải hiển thị thông tin sách, không phải một đoạn văn |
| 26 | Không có | Bảng `CONFIG` lưu toàn bộ thiết lập người dùng; tách rõ option nào **làm thay đổi token stream** (→ rebuild) và nào **chỉ đổi tập file quét** (→ rescan) (§3.5.3) | Đổi config nhạy cảm mà không rebuild = index sai âm thầm |
| 27 | `.az` coi là "giống EPUB" | **Đính chính**: MOBI/AZW/AZW3 là **PalmDB + PalmDOC LZ77**, *không* phải ZIP. Payload bên trong là HTML (giống EPUB), nhưng container khác hoàn toàn (§3.0.2) | Viết parser sai container = không đọc được gì |
| 28 | Query chỉ khớp term đầy đủ | **Fallback prefix expansion** (§4.1.1): term không có trong `DICT` thì mở rộng thành prefix | Gõ `cha` không ra `chai` — trên bàn phím PW3 thì gõ từ một phần là hành vi chủ đạo, không phải edge case |
| 29 | Không có | `prefix_search_mode` (`auto`/`exact`/`prefix`) + đánh dấu `fuzzy` + hiển thị cho user biết từ đã bị mở rộng | Không báo thì user không hiểu sao gõ `cha` lại ra sách không có chữ "cha" |
| 30 | Mọi con số kích thước đều là ước tính (±50%) | **Số đo thật trên 614 EPUB** (§1.1.1): 1,36GB EPUB → 348,7MB text → 56,3M token → **index full positional 102,9MB (7,6% EPUB)** | Gate §7.7 đã chạy. Suy rời cũ thổi phồng ~5,8× do đoán sai mật độ token/thư viện |
| 31 | Kiến trúc đề xuất "2 tầng + xác minh" vì sợ index quá lớn | **Không cần thiết** — full positional chỉ 102,9MB. Chỉ cần khi thư viện > ~3,5GB | Không thêm phức tạp khi số đo cho thấy không cần |
| 32 | `zstd-19` cho đường build | **`zstd` level 3** (§3.4 L3): 6,5s vs 512s trên 348,7MB, chỉ nhỏ hơn 32% | Level 19 trên Cortex-A9 mất 1,5–3 giờ mỗi lần build |
| 33 | redb "2.x" | **redb 4.3.0** (đã kiểm tra API): `Builder::set_cache_size()`, key/value cho `(&[u8], u16)`, **không có nén native** | Crate đã lên 4.x; `set_cache_size` chỉ có trên `Builder`, `len()` chỉ có trên *untyped* table, `iter().next()` trả `Option<Result<..>>` |
| 34 | "đọc bảng chưa tạo = lỗi" | **`TableDoesNotExist` = bảng rỗng**, xử lý tập trung ở `storage::or_empty()` | DB mới tạo hoặc user chưa bật indexing là trạng thái bình thường, không phải lỗi |
| 35 | Scan một lần cho mọi root | **Hai chế độ: `scan_scope(dir)` và `scan_full(config)`**, `ScanMode::{Scope,Full}` (§3.1.1) | User gõ search khi đang mở thư mục con → index đúng thư mục đó (vài giây), không quét cả thư viện (vài phút) |
| 36 | Search luôn trên toàn bộ DB | **`ScopeFilter(dir)`** giới hạn kết quả theo path prefix | Search ở thư mục con không được lẫn sách của thư mục khác |
| 37 | Sách `missing` xét trên toàn bộ DB | Chỉ xét sách nằm dưới root **đọc được trong lượt này** | Không guard này thì full scan 1 root sẽ xóa sạch sách đã scope-index ở root khác |
| 38 | Dùng crate `mobi` 0.8.0 phân tích MOBI | **Tạm bỏ qua MOBI/AZW/AZW3** | Crate `mobi` (duy nhất) không cập nhật từ 2022-12, phụ thuộc `encoding` 0.2 (đã bị bỏ rơi), doc coverage 21,9%, `content_as_string()` trả 1 String → **mất byte offset** mà `ENTRYMAP.checkpoints` cần. Chỉ 5/614 cuốn (0,8%) |
| 39 | Sniff đọc 64 byte đầu | Đọc **128 byte** | Magic `BOOKMOBI` nằm ở offset 60..68 — đọc 64 byte sẽ cắt đôi magic và mọi MOBI thành `Unknown` |
| 40 | So `<!doctype html` theo hằng byte | So **không phân biệt hoa thường** | `<!DOCTYPE html>` là chuẩn nhưng file thật có `<!Doctype Html>`, `<!doctype html>` |
| 41 | Chỉ parse `Event::Start` | Parse cả `Event::Start` **và** `Event::Empty` | `<item/>` và `<itemref/>` trong OPF là **self-closing** → chỉ parse `Start` thì **spine rỗng hoàn toàn**, mọi EPUB mất thứ tự đọc |
| 42 | Lấy text bằng handler `text!("body")` | Dùng handler `element!("*")` + `remove_and_keep_content()` | Handler `text!` cũng bắt text **bên trong** `<script>` (là con của `body`) → rò rỉ JS vào index |
| 43 | `&` không có `;` thì giữ tối đa 32 byte | State machine 3 trạng thái: `Complete` / `NotEntity` / `Truncated` | `Tom & Jerry` có `&` rồi gặp space → phải giữ lại literal ngay, không thì 1 dấu `&` nuốt 32 byte |
| 44 | Tin `media-type` của manifest để biết entry nào là nội dung | **Tin đuôi file**: `.xhtml`, `.html`, `.htm`, `.xhtm` (§3.2) | `media-type` hay bị thiếu hoặc ghi sai trong EPUB thật; đuôi file trong `href` thì luôn có |
| 45 | So tên element/attribute OPF **phân biệt hoa thường** | `local_name()` và `attr_value()` so **`eq_ignore_ascii_case`** | HTML/XHTML không bắt buộc hoa hay thường — `<Title>`, `<ItemRef>`, `<OPF:SCHEME>`, `IDREF` là hợp lệ |
| 46 | Tra entry ZIP **phân biệt hoa thường** | `read_entry` fallback quét case-insensitive | href `ChapterOne.XHTML` không khớp entry `chapterone.xhtml` → section bị bỏ **âm thầm** |
| 47 | `<?xml`, `<![CDATA[` phân biệt hoa thường | `eq_ignore_ascii_case` | File thật có `<?XML`, `<![cdata[` |
| 48 | Không có module tokenize — whitespace còn nguyên trong text | Module `tokenize` với `for_each_token()` (§3.2.1) | `\r`, `\n`, `\t`, nbsp đều phải là dấu tách; trước đó chúng còn nằm trong `Section.text` |
| 49 | Chỉ NFD + bỏ combining mark | Thêm fold tay `đ`/`Đ` → `d` | `đ` (U+0111) là chữ cái riêng, NFD **không** tách — không fold thì gõ `duoc` không ra `được` |
| 50 | `is_alphanumeric()` cho chữ+ số chung một run | Chữ và số tách nhau | `Chapter12` phải ra `chapter` + `12` để tìm `3` ra `Tập 3` |
| 51 | Fold dấu một chiều → preview mất | `for_each_token_spanned()` trả byte range trong **text gốc** (§3.2.3) | `OffsetMap` map ngược từ byte đã normalize về byte gốc |
| 52 | `src` cộng theo char trong lúc NFD | Cộng theo **char gốc**, mọi byte sinh ra từ char đó map chung về `char_start` | Span rơi vào giữa ký tự → slice panic. NFD của U+1EE7 cho ra 3 char, cộng dồn đỗ ra byte 57 giữa chữ `ủ` |
| 53 | **Flush postings sau mỗi book** → 1 chunk cho mỗi (term, book) | Tích lũy **xuyên book**, chỉ ghi khi pool vượt `MAX_PENDING` (4M posting) | Thư viện thật: 743.587 chunk / 62.663 term → output 492MB. Sau fix: **71.619 chunk** — vừa đủ để mỗi term đạt đúng `CHUNK_POSTINGS` |
| 54 | Khởi tượng không biết redb giữ free page | `Engine::compact()` sau session đã đóng writer | Đo thật: file mới ghi 293MB nhưng **dữ liệu thô chỉ 91,5MB** → 69% là trang rác. Compact là bắt buộc, không phải tùy chọn |
| 55 | Đọc chunk theo thứ tự **key** | Sắp xếp theo `Chunk.first_doc` đã giải mã | Key `(term, chunk_idx: u16)` — redb mã hoá u16 **big-endian**, nên term >256 chunk iterate sai thứ tự (chunk 256 đứng trước chunk 1) → position lùi, phrase query **mất hit âm thầm**. Bắt bởi test thư viện thật |
| 56 | Restart position 0 cho **mỗi section** | Một bộ đếm xuyên suốt cả book, `entry_idx` phân biệt section | `pos_gap` là delta: reset mỗi section làm position **trùng lặp** trong cùng document → underflow → chunk decode hỏng. Triệu chứng: search trả 0 hit toàn bộ |
| 57 | PalmDOC tự viết sai 3 quy tắc | `0x01..=0x08` = đếm N literal từ stream (không lặp ký tự trước); `0x80..=0xBF` item 14-bit = `((b&0x3F)<<8)\|next` → dist `item>>3`, len `(item&7)+3`; `0xC0..=0xFF` ra `' ' + (b^0x80)` | Bị bắt nhờ hexdump file thật: `0xEF` → `" o"`, `0xC3` → `" C"`. Verify: cả 5 file MOBI decode sạch, tiếng Việt đúng dấu |
| 58 | Indexer không ghi `BOOKS` | Ghi `BOOKS` cho mọi book cùng lúc với metadata | `rebuild_index()` chỉ gồm book có `BOOKS` sống → thiếu row là metadata **biến mất âm thầm** khỏi mọi metadata search. Phải gán cùng book_id cho postings và metadata |
| 59 | MOBI không có đường metadata | `from_embedded()` đọc EXTH cho Mobi/Azw/Azw3 qua `format::mobi` | User yêu cầu index metadata mọi định dạng; text không cần giải nén cũng lấy được title/authors |
| 53 | `.nfd()` áp lên toàn chuỗi | NFD **theo từng char**, và **miễn trừ Hangul syllable** | Hangul decompose thành jamo (không phải combining mark) → `한국어` rách, và NFKC per-char không tái hợp được |
| 54 | Định bỏ position vì "preview vẫn phải lục lại epub" | **Giữ position chính xác** (§3.4 L1.2) | Lục lại là **seek** chứ không quét: 3ms (1 section) vs 18ms (cả cuốn). Không có position thì tìm "tham tu" phải extract mọi sách ứng viên ≈ 9 giây |
| 55 | Span có thể rơi giữa ký tự đa byte | Test `real_book_slices_are_always_on_char_boundaries` trên 40 cuốn thật | Unit test không tái hiện được; chỉ xuất hiện trên text thật |
| 56 | `remove_book` chỉ xóa `BOOKS` + `PATHS` | Xóa thêm row `METADATA`, và `rebuild_index` chỉ lấy id còn trong `BOOKS` | `METAIDX` rebuild từ row `METADATA` → row sót lại làm sách đã xóa **sống lại trong mọi lần search metadata, vĩnh viễn**. Postings thì orphan bị lọc khi đọc được, metadata thì không |
| 57 | `MetaIndex::push` đòi caller normalize | `push` và `search` **tự normalize** | Quên một lần → index âm thầm không khớp gì, không có lỗi báo |
| 58 | `sequence` FB2 là `Event::Empty` | Gộp `Start | Empty` (lần thứ 2 của cùng một lỗi) | Lỗi lặp lần 3 sau OPF `itemref`/`item`: self-closing element bị bỏ hoàn toàn, lần này mất **mọi series của FB2** |
| 59 | Tin `media-type` để biết entry nào là nội dung | **Tin đuôi file** (§3.2.2) | `media-type` hay thiếu hoặc sai trong EPUB thật; `href` thì luôn có |
| 60 | Element/attribute OPF phân biệt hoa thường | `eq_ignore_ascii_case` | `<Title>`, `<ItemRef>`, `OPF:SCHEME`, `IDREF` là hợp lệ → mất metadata + mất spine |
| 61 | Preview lấy span của token cuối | Nhớ `candidate_start` từ token đầu | `push()` hoàn tất match chỉ nhận span của token **cuối** → preview ra `"tử"` thay vì `"thám tử"` |

#### 3.2.1 Bộ tokenize — những chỗ dễ làm sai

Module `tokenize` thực thi §3.2 bước 4–10. Ba quyết định không hiển nhiên:

**① `đ` không tách được bằng NFD — phải fold tay.** `đ` (U+0111) là chữ cái
**riêng**: NFD cho ra chính nó, không có decomposition. Nếu chỉ dựa vào NFD thì
người dùng gõ `duoc` trên bàn phím Mỹ **không bao giờ** tìm thấy `được`.
`fold_letter()` map riêng `\u{0110}`/`\u{0111}` → `d`.

> **`đ` và `ơ` KHÁC bản chất** — đã kiểm chứng bằng code, không phải suy đoán:
> `NFD('ơ') = ['o', '\u{31B}']` — sừng là **combining mark có mã riêng**, nên
> bị bỏ bởi vòng NFD thông thường. `NFD('đ') = ['đ']` — gạch ngang **không có
> decomposition nào cả**. Cùng cơ chế với `ư` (u + U+031B), `ă` (a + U+0306),
> `â ê ô` (a/e/o + U+0302).
>
> Test `every_vietnamese_letter_collapses_to_one_ascii_letter` quét toàn bộ chữ
> Việt (14 chữ riêng + U+1EA0..U+1EF9) và xác nhận **không** ký tự nào còn lại
> dạng non-ASCII. Nếu bỏ `fold_letter` đi, `đ`/`Đ` sẽ hiện ngay trong danh sách đó.

**Hai hệ quả phải ghi nhận (không phải bug):**

- **Bỏ thanh điệu làm trùng từ.** `ta tá tà tả tã tạ` (tôi, đập, rơi, trái,
  cả, tạ) đều gộp về `ta`. Trong tiếng Việt thanh điệu có tính phân biệt nghĩa.
  Đây là cái giá của §0 #9 (để người không có bàn phím Việt vẫn tìm được sách).
  Cách sửa đúng là index **cả hai dạng** (folded + nguyên bản), nhưng vậy làm gấp
  đôi term dictionary.
- **Ảnh hưởng cả tiếng Bắc Âu.** `å ä ö` **có** decompose (a/o + ký tự kết hợp)
  nên cũng bị gộp: `år` (năm) và `ar` trùng nhau. Riêng `ø æ ß ł ð þ` không
  decompose nên giữ nguyên. Vô hình trên thư viện tiếng Việt, nhưng phải biết
  trước.

**② Chữ và số tách nhau (`Chapter12` → `chapter`, `12`).** `char::is_alphanumeric()`
trả `true` cho cả hai, nên một run đơn thuần sẽ dính chúng lại. Người dùng tìm
`3` phải ra `Tập 3` — nên run đổi lớp (chữ ↔ số) sẽ bị chặn và flush.

**③ Mọi ký tự không phải word-char đều là dấu tách.** Cụ thể gồm `\r`, `\n`,
`\t`, `\u{000B}`, `\u{000C}`, `\u{00A0}` (nbsp), `\u{2028}`, `—`, `_`, `-`,
`'`, dấu câu. Không token nào có thể chứa whitespace; `term_is_clean()` kiểm tra
điều này.

Thứ tự normalize là **bắt buộc**: NFD → bỏ mark → `\u{0111}` fold → lowercase → NFKC.
Làm NFKC trước sẽ tái hợp chuỗi và các dấu kết hợp không còn nhận ra được.

Position là bộ đếm monotone trong **toàn bộ sách**, liên tục qua các section
(`for_each_token` trả về position cho chunk kế tiếp). CJK đếm **theo ký tự**,
không theo byte: `中文测试` → unigram ở 0,1,2,3 và bigram ở 0,1,2.

Đã kiểm chứng trên thư viện thật (`bin/tokcheck`): 25 cuốn → **3.091.495 token,
0 token xấu**, ~123K token/cuốn, ~550K token/giây.

#### 3.2.3 Phrase query — đã kiểm chứng trên thư viện thật

Câu hỏi then chốt: *"thám tử" có khớp được tài liệu ghi "Thám tử lừng danh"
không?* **Có.** Test `tests/phrase_query.rs` chạy đủ pipeline (extract → tokenize
→ positional intersect) trên 120 cuốn thật:

| Query | Sách khớp | Preview (nguyên văn) |
|---|---|---|
| `thám tử` (có dấu) | 64 | `"thám tử"` |
| `Thám tử` (hoa khác) | 64 | `"thám tử"` |
| `THÁM TỬ` (hoa toàn bộ) | 64 | `"thám tử"` |
| `tham tu` (**không dấu**) | 64 | `"thám tử"` |
| `Thám-tử` (dấu tách khác) | 64 | `"thám tử"` |
| `tử thám` (đảo thứ tự) | 29 | `"từ thâm"` |
| `thám` (một từ) | 120 | `"thấm"` |
| `thám tử lừng danh` (3 từ) | 8 | **`"thám tử lừng danh"`** |
| `sherlock holmes` | 14 | `"Sherlock Holmes"` |
| vô nghĩa | 0 | — |

Bốn thuộc tính được xác nhận bằng số liệu, không phải bằng lập luận:

1. **Bỏ dấu không đổi tập kết quả** — 64 == 64. Đây là yêu cầu chính.
2. **Hoa/thường không ảnh hưởng** — cùng 64 cho cả 3 kiểu viết.
3. **Dấu tách giữa các từ không ảnh hưởng** — `Thám-tử` cho cùng kết quả, vì
   `-` chỉ là separator và position vẫn liền nhau.
4. **Thứ tự có ý nghĩa** — `tử thám` cho 29 cuốn, ít hơn rõ rệt.

Hai quan sát đáng chú ý trong bảng:
- Query `thám` match cuốn ghi **`thấm`** — đúng như hệ quả bỏ thanh điệu đã ghi
  ở §3.2.1 (hai từ khác nhau gộp về một key). Đây là bằng chứng thực tế cho
  cái giá đã biết đó.
- Preview luôn là **nguyên văn của sách**, không bao giờ là key đã fold.

Matching chạy streaming (O(1) bộ nhớ) chứ không giữ cả token stream: một cuốn
~124K từ, và bản đầu của test này vật chất hóa chúng thành `String` riêng nên
mất 5 phút; bản streaming mất **15 giây**.

#### 3.2.5 Provenance — đã kiểm chứng trên thư viện thật

Thứ tự ưu tiên (§2.3): sidecar KOReader → metadata nhúng → tên file.

Kiểm chứng trên thư viện thật: **43/43** cuốn trong mẫu 150 có sidecar
`metadata.epub.lua`, nên gần như luôn là sidecar quyết định. Điều này không phải
chi tiết vặt: một số OPF trong thư viện ghi `<dc:language>en</dc:language>` cho
cuốn sách tiếng Việt, trong khi sidecar ghi `text_lang = vi`. Tin OPF thì search
sai — sidecar là thứ người dùng đang thấy nên phải thắng.

Sidecar là Lua, nhưng ta **không** nhúng một Lua engine: chỉ đọc subset mà
KOReader viết (`["key"] = value`, lồng 1 cấp, chuỗi có line-continuation).
Chạy Lua tuỳ ý từ thư mục sách là rủi ro lớn hơn nhiều so với giá trị của tính năng.

#### 3.2.6 Preview kết quả không bị mất

Lo ngại hợp lý: fold dấu + lowercase là **mất mát một chiều**, nên liệu có còn
hiển thị được "Thám tử" khi user gõ "tham tu"?

Phân biệt rõ:

| Thành phần | Trạng thái |
|---|---|
| Text gốc trong sách | **Không bao giờ bị ghi đè** — `Section.text` giữ nguyên hoa/thường và dấu |
| Key đánh index | Đã normalize (`"Nguyễn"` → `"nguyen"`) |
| Position của token | Có |
| **Offset của token trong text gốc** | Có, qua `for_each_token_spanned()` |

`for_each_token_spanned()` trả thêm `(start, end)` — byte range trong **text gốc**,
đó là thứ cần để cắt cửa sổ snippet. `note.md` §4.5 mô tả cách dùng:
lấy position → tìm span → cắt ±7 token quanh đó → hiển thị nguyên văn.

Hai bug đã bắt khi làm phần này (xem §0 #52, #53):
- `src` cộng theo **char trong lúc NFD** → span rơi vào giữa ký tự, slice panic.
- `.nfd()` làm rách **tiếng Hàn** thành jamo và không tái hợp được.

Ví dụ thật: query `"nguyen"` → key `"nguyen"` → preview
`"Thẩm phán, công tố viên và nguyên đơn đứng ở một khoảng cách khá"` — đúng
nguyên văn có dấu, không phải key đã fold.

#### 3.2.2 Quy tắc "tin đuôi file"

Một manifest entry được coi là **nội dung** nếu:

```
nếu href kết thúc bằng .xhtml / .html / .htm / .xhtm  → là nội dung  (không phân biệt hoa thường)
ngược lại nếu href KHÔNG có đuôi                    → hỏi media-type
ngược lại                                           → không phải nội dung
```

Nhánh cuối là chặn quan trọng: không có đuôi **và** không có `media-type` hợp lệ thì ảnh,
CSS, font không bao giờ lọt vào index. `media-type` chỉ là dự phòng, không bao giờ ghi đè
đuôi file.
| 35 | `config_hash` gồm `min_file_size` | Chỉ gồm setting đổi **token stream** (`index_depth`); mọi setting đổi **tập file quét** ở ngoài | `min_file_size` chỉ ảnh hưởng sách nào được quét → rescan, không phải rebuild |

---

## 1. Mục tiêu & Ràng buộc

### 1.1 Quyết định Q1: chọn **phương án (a)** — full positional index

#### 1.1.1 Số đo thật (đã chạy — gate §7.7, không còn là ước tính)

Đo trên **toàn bộ 614 EPUB** trong `~/.config/koreader/rakuyomi/downloads/calibre`,
không suy rọi (`cargo run --release --bin measure`):

| Hạng mục | Giá trị đo được |
|---|---|
| Thư mục `.epub` trên đĩa | **1,36 GB** |
| Text sau khi trích xuất (raw UTF-8) | **348,7 MB** (25,6% của EPUB) |
| Token = posting (gồm stopword) | **56,3 triệu** |
| Term riêng biệt | 70.600 |
| Dictionary (front-coded) | **401 KB** |
| **Index full positional** | **102,9 MB = 7,6% của EPUB** |

Hiệu năng build (host x86, để suy ra thứ bậc trên PW3):
- Extract 614 cuốn: **63 s**
- Encode index full: **5,2 s**

**Kết luận: phương án (a) hoàn toàn khả thi.** Với thư viện 1,36GB thì index full chỉ 102,9MB
trên phân vùng user ~2,7GB của PW3. Điều kiện duy nhất là thư viện không vượt quá
**~3,5GB** (102,9MB × 3,5/1,36 ≈ 265MB), vẫn nằm trong ngân sách §1.2.

Hiệu ứng của việc giữ position (`cap` = số position tối đa mỗi (term, sách)):

| cap | positions giữ lại | Index | % EPUB |
|---|---|---|---|
| **full** | 56,3 M (100%) | **102,9 MB** | 7,6% |
| 32 | 18,8 M (33%) | 39,5 MB | 2,9% |
| 8 | 8,5 M (15%) | 20,1 MB | 1,5% |
| 2 | 3,2 M (6%) | 8,7 MB | 0,6% |

**Mặc định: `cap = full`.** Kiến trúc "2 tầng + xác minh" tôi từng đề xuất **không cần thiết**
cho thư viện này — nó chỉ trở nên đáng làm khi thư viện > 3,5GB. `cap` vẫn để user chỉnh
được (`index_depth`) để user có thư viện lớn có thể tự hạ xuống 32 hoặc 8.

#### 1.1.2 Các con số ước tính cũ đã bị đo bác bỏ

| Giả định trong bản cũ | Đo thật | Hệ quả |
|---|---|---|
| 2,5GB thư viện → 600M token | 1,36GB → 56,3M token (`~24 byte EPUB/token`, không phải ~4) | Phép suy rọi cũ thổi phồng kích thước gấp ~5,8 lần |
| `pos_gap` ~5,5 bit/posting | **11,70 bit/posting** | Không coder (BIC/PFor/EF) nào qua mặt được: khoảng cách trung bình giữa hai lần xuất hiện ~3300 token |
| `doc_gap` ~2,5 bit/posting | **0,99 bit/posting** | Rẻ hơn đoán, vì khối lượng posting tập trung ở từ tần số cao (cột doc dày đặc) |
| `zstd-19` cho đường build | **zstd-19: 512 s / 348MB** trên host; `zstd-3`: **6,5 s** | zstd-19 trên PW3 sẽ mất vài giờ → **dùng zstd-3** (§3.4 L3) |

**Vì sao `pos_gap` ăn 84% index:** position phải lưu tuyệt đối trong một dải dài
(~92 nghìn token/sách), nên mỗi gap mang ~11,7 bit entropy. Đây là sàn của chính
bài toán phrase query, không phải điểm yếu của bộ nén.

- Không cắt ngắn nội dung sách, không dùng block-index → **phrase query chính xác 100%**.
- **Stopword được index** (lọc stopword thì "to be or not to be" không bao giờ khớp).
- DB chiếm ~7,6% dung lượng thư viện đối với thư viện đo được → rất thoải mái (§1.2 vẫn giữ
  làm vđệm phòng thư viện lớn hơn nhiều).

### 1.2 Ngân sách đĩa (bắt buộc, không tùy chọn)

```
FREE_SPACE_FLOOR = 256 MB      # KOReader cần chỗ cho cache/screenshot/temp
DB_BUDGET       = min(25% × library_bytes, 700 MB)
# thư viện đo được: 1,36GB × 25% = 340MB >> 102,9MB thực tế → chưa bao giờ chạm ngưỡng
```

- Trước mỗi sách: nếu `free_space < FREE_SPACE_FLOOR` hoặc `db_size + ước_lượng_sách_tới > DB_BUDGET` → **dừng index**, set `status = budget_exceeded`.
- UI hiển thị: "% thư viện đã index", "DB đang chiếm X MB", nút tắt indexing.
- Thứ tự ưu tiên khi sắp hết ngân sách: **mới thêm gần đây nhất trước** (`ORDER BY mtime DESC`).
- Tùy chọn nữa nếu vẫn quá chặt: setting `index_depth` — `full` (mặc định) / `cap=32` / `cap=8` (§1.1.1). Đây là **công tắc chống đói dung lượng** do user chủ động bật, không phải mặc định. Với thư viện đo được thì `full` chỉ 102,9MB nên gần như không ai cần đụng tới nó.
- Tùy chọn setting: `index_only_when_charging` (mặc định bật — tránh cạn pin).

### 1.3 Quy ước mã nguồn: **English only**

**Toàn bộ code, comment, doc-comment (`///`), tên identifier, tên module, message log, message error, tên test và commit message phải viết bằng tiếng Anh.**

**Duy nhất `note.md` và `docs/*.md` là tiếng Việt.**

Lý do:
- Error message của ta đi thẳng vào log KOReader (người dùng paste khi báo lỗi) → phải grep/tra cứu được.
- Output của `rustc`, `clippy`, `rustdoc`, `cargo`, mọi backtrace đều English; lẫn tiếng Việt vào log làm truy vết rối.
- LuaJIT FFI là biên ngôn ngữ C — tên hàm và mô tả phải đọc được từ cả hai phía.
- Nếu sau này có contributor ngoài hoặc open-source phần crate.

Cách áp dụng:
- Cấm string literal tiếng Việt trong `src/` (kiểm tra trong review).
- `error`/`log` message viết theo mẫu: `"turbosearch: <subsystem>: <what happened>: <actionable detail>"`.
- CI chạy `cargo clippy --all-targets -- -D warnings` và `cargo fmt --check`; review checklist có 1 dòng "no non-English strings in src/".
- Tên biến kiểu `book_id`, `doc_gap`, `entry_idx` — dùng thuật ngữ information-retrieval tiếng Anh chuẩn, **không** dịch sang tiếng Việt rồi viết tắt.

```rust
// Good
/// Sparse map from token position to byte offset in the raw entry.
/// Checkpoints are emitted every CHECKPOINT_INTERVAL positions.
pub struct Checkpoints { pub interval: u32, pub entries: Vec<(u32, u32)> }

// Bad — never do this
/// Bảng checkpoint: vị trí token -> offset byte trong entry thô
```

### 1.4 Mục tiêu hiệu năng (đo trên máy thật, không đoán)

| Chỉ số | Mục tiêu |
|---|---|
| Scan metadata 6.000 file | < 50 ms |
| Index 1 EPUB 400KB | < 800 ms (nặng hơn v2 vì phải nén) |
| Index toàn thư viện 2.5GB (lần đầu) | 15–40 phút, resume được |
| Phrase query 2–3 từ | p50 < 10 ms, **p95 < 50 ms** |
| Query có prefix expansion (§4.1.1) | p95 < 80 ms — ngân sách rộng hơn vì phải decode nhiều term hơn (cap 100K posting tổng) |
| RAM resident lúc query | < 40 MB (dictionary ~10–15MB trong đó) |

---

## 2. Mô hình dữ liệu (redb 2.x)

10 bảng. Toàn bộ value là **byte packing tự viết** (không serde/bincode) để đổi schema là chủ đích.

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

### META — 1 entry duy nhất (`key = "meta"`)
```
schema_version:     u32 = 3
tokenizer_version:  u32 = 1
normalizer_version: u32 = 1
codec_version:      u32 = 1     # phiên bản của khối nén §3.4
config_hash:        u32        # hash của cấu hình ảnh hưởng index
book_id_hwm:        u16        # id kế tiếp được cấp
dict_size_estimate:u32
db_bytes_written:  u64
status:             u8         # idle | scanning | indexing | budget_exceeded
scan_cursor:       [u8]       # path đã xử lý gần nhất (resume)
```
Bất kỳ version hoặc `config_hash` nào lệch với bản build hiện tại → **xóa DB và rebuild từ đầu** (an toàn hơn cố đọc format cũ).

### BOOKS — key `book_id: u16`
```
scope_len: u16, scope: [u8]     # root đã phát hiện ra sách này (§3.1.1)
path_len: u16, path: [u8]
size: u64, mtime: i64, inode: u64      # inode CHỈ để debug, không dùng so khớp
format: u8                              # epub|txt|fb2|azw3
entry_count: u16
token_count: u32
indexed_at: i64
flags: u8                               # bit0 = partial (bị cap), bit1 = orphan
```

### PATHS — key `path` → `book_id:u16`
Reverse lookup O(1) cho scan.

### METADATA — key `book_id: u16` → value
Thông tin mô tả sách, dùng để **hiển thị** kết quả. Không dùng cho matching.

```
title:        [u8]                     # như trong sách, giữ nguyên dấu/hoa
subtitle:     [u8]
authors:      [ [u8] ]                 # đã tách, dedup
creators:     [ [u8] ]                 # editor, translator, illustrator, narrator…
language:     [u8]                     # BCP-47 lowercase, "vi", "en", "zh-CN"
publisher:    [u8]
year:         i32 (0 = không rõ)
series:       [u8]
series_index: u32 (×1000, 0 = không rõ)
identifiers:  [ (scheme:u8, value:[u8]) ]   # isbn, doi, uuid, asin, uri, goodreads
tags:         [ [u8] ]                 # subject/genre/keyword
description:  [u8]                     # cap 4 KB, bit `desc_truncated` nếu cắt
word_count:   u32
source_format: u8                      # Format enum, §3.0.1
provenance:   u8                       # embedded | sidecar | filename
flags:        u8                       # bit0 = desc_truncated, bit1 = meta_only
```

**Provenance quan trọng:** quyết định dùng metadata từ đâu, theo thứ tự ưu tiên:
1. **Sidecar của KOReader** (`<book>.sdr/metadata.epub.lua`) nếu tồn tại → đây là thứ người dùng **đang thấy** trong thư viện KOReader, dùng lại cho nhất quán. Cần một mini parser cho *subset* Lua bảng phẳng (`key = "string"`, `key = 123`, `key = true`) — ~80 dòng, không phải Lua engine. *(Tùy chọn, phase 3 — nếu làm thì rất đáng.)*
2. **Metadata nhúng trong file** (EPUB OPF / MOBI EXTH / FB2 `<description>`).
3. **Tên file** (bỏ đuôi, tách `-`/`_`) — cho `.txt`/`.mobi` không có metadata.

### METAIDX — **1 KV duy nhất** (`key = "metaidx"`)
**Đây mới là bảng phục vụ tìm kiếm metadata.** Lý do không dùng postings cho metadata: postings chỉ khớp được **cả token** (hoặc prefix), còn metadata cần khớp **chuỗi con** — người dùng gõ `nguy` phải ra `Nguyễn`, gõ `imov` phải ra `Asimov`. Khớp chuỗi con trên inverted index là không thể.

```
[n_books: u32]
với mỗi sách: [book_id: u16][blob_off: u32][blob_len: u16]     # index song song với blob
[blob…]          # blob đã normalize (cùng normalizer §3.2), nối tiếp nhau, zstd-19 toàn khối
```

Nội dung blob mỗi sách (đã normalize, phân cách bằng `\n`):
```
title\nauthor1\nauthor2\nseries\ntag1\ntag2\npublisher\nyear\nfilename_stem\ndescription
```

**Vì sao là 1 KV duy nhất thay vì 6000 KV:** đọc 6000 blob riêng lẻ = 6000 lần `get()` = 6000 lần duyệt B-tree page. trên Kindle đó là 100–300 ms. Gộp 1 KV ~1.5–2MB nén, nạp RAM 1 lần (LRU 1) rồi quét bằng `memmem` → **< 3 ms** cho cả thư viện (6000 × ~300 byte = 1.8MB, quét tuyến tính có SIMD).

`METAIDX` được **rewrite toàn bộ 1 lần mỗi session index** (nén 2MB mất ~100 ms — rẻ hơn nhiều so với việc duy trì delta table).

### DICT — **1 KV duy nhất** (`key = "dict"`)
Toàn bộ term dictionary, đã **front-coded + zstd**.

```
[n_terms: u32] = N
[m_index_entries: u32]
với mỗi block 32 term (front-coded):
  mỗi term: [u8 prefix_len][u8 suffix_len][u16 df][u32 tf][u16 chunk_count][u8 flags][suffix bytes]
```
Header `m_index_entries` là sparse index cho binary search: mỗi entry trỏ tới block đầu tiên có term ≥ một ngưỡng cho trước, nằm trong vùng **đã giải nén**. Build script sinh sparse index sao cho mỗi entry cách nhau ~64 entry.

Vì sao 1 KV thay vì 1M KV (§3.4 L4): redb lưu mỗi cặp KV trong leaf page 4KB. ~1M term × (key 10B + value 12B + overhead) ≈ 30–50MB chỉ cho overhead trang. Một KV ~5MB nén loại bỏ hoàn toàn phần này, và prefix search trở thành binary search trong RAM.

### DICT_DELTA — key `term` → value nhỏ
Chỉ chứa thay đổi của session index đang chạy: `df_delta`/`tf_delta`/`chunk_count`. Khi session đóng (hoặc compact), merge vào `DICT` và **rewrite `DICT` đúng 1 lần**, rồi xóa `DICT_DELTA`. Tránh việc rewrite cả dictionary (vài MB nén) sau mỗi sách.

Prefix search khi đang merge: dò cả `DICT` và `DICT_DELTA` (delta nhỏ, rẻ).

### POSTINGS — key `(term, chunk_idx: u16)` → value
```
[u16 first_book_id]     # binary-search chunk mà không cần bảng index phụ
[u32 codec_tag]         # magic + version của khối nén
[bytes nén]             # §3.4 L1..L3
```
**Chunk ~8192 posting (~7–8KB nén).** Chunk nhỏ là có chủ đích: `get()` trong redb đọc cả value → chunk nhỏ cho phép decode lazy, binary-search rẻ, và bỏ được page 4KB.

**Tính chất then chốt:** `book_id` tăng đơn điệu (high-water-mark). Không bao giờ tái sử dụng id → index lại sách luôn sinh id **lớn hơn** mọi id cũ → chunk mới luôn nằm sau chunk cũ theo thứ tự `(book_id, pos)`. **Append-only do đó không phá vỡ thứ tự toàn cục**, hai con trỏ intersect vẫn chạy tuyến tính. Posting mồ côi nằm lẫn trong dãy, bị bỏ qua khi intersect.

### ENTRYMAP — key `(book_id, entry_idx)` → value
Gộp vai trò của `ANCHORS` (v2) và bảng checkpoint mới:
```
entry_name: [u8]
byte_len:   u32
xpath_hint: [u8]              # id/class ổn định nếu có, để dựng Locator nhanh
checkpoints: [u8]             # delta+PFor: mỗi 2048 token một cặp (pos, byte_off)
```

### CONFIG — thiết lập người dùng (§3.5)
```
source_dirs:           [ [u8] ]    # mặc định ["/mnt/us/documents"]
include_extensions:    [ [u8] ]    # §3.5.1
exclude_extensions:    [ [u8] ]    # §3.5.1
exclude_dir_globs:     [ [u8] ]    # §3.5.2
min_file_size:         u64         # mặc định 5120
max_file_size:         u64         # mặc định 64 MiB
index_fields:          u32 bitmask # field metadata nào vào METAIDX blob
longtail_formats:      u8          # bật crate ebook-rs: RTF/ODT/DOCX/LIT/KFX
meta_only_unparseable: u8          # PDF/DJVU vẫn index metadata (mặc định tắt)
prefix_search_mode:    u8          # auto (mặc định) | exact | prefix  — §4.1.1
```

Toàn bộ đổi CONFIG đi qua Lua side → FFI `config_set()` / `config_get()`. Rust không đọc file config, không nhúng sẵn đường dẫn KOReader.

> **§2.7 Quy tắc bảng "1 KV duy nhất"** (`META`, `DICT`, `METAIDX`): nạp RAM khi cần, LRU 1. Khác nhau ở **dữ liệu** và **nhịp độ rewrite**: `META` ghi mỗi lần scan (vài trăm byte), `DICT` merge 1 lần/session, `METAIDX` rewrite 1 lần/session. Cả 3 đều nằm ngoài đường nóng của query sau khi đã nạp.

---

## 3. Pipeline

### 3.0 Nhận diện format & lọc đuôi file

Hai bước, có chủ đích khác nhau — **đừng lẫn**:

```
bước 1  extension pre-filter  →  quyết định có ĐỌC file không (rẻ, không mở file)
bước 2  magic sniff (64 byte) →  quyết định format THỰC (mở file, đọc 64 byte)
```

**Extension chỉ là pre-filter tốc độ; magic byte mới là trọng tài.** Lý do: trong thư viện Kindle thật, `.azw`/`.azw3`/`.mobi`/`.kfx` đặt tên lộn xộn không đồng nhất, và file hay bị đổi tên sai đuôi. Nếu chỉ tin extension, ta sẽ bỏ sạch một nhóm sách mà không báo lỗi gì.

#### 3.0.1 Format enum & magic

| Format | Magic / dấu hiệu | Nguồn text | Nguồn metadata | Phase |
|---|---|---|---|---|
| **EPUB2/3, KEPUB** | `PK\x03\x04` + entry `mimetype` = `application/epub+zip` | entry `.xhtml`/`.html` theo OPF `<spine>` → `lol-html` | OPF `<metadata>` | 1 |
| **MOBI / AZW (KF7) / AZW3 (KF8)** | `BOOKMOBI` tại offset **60..68** của PDB header | giải PalmDOC LZ77 → HTML → `lol-html` | **EXTH records** (updated title, creator, publisher, description, subject, isbn, language) | 2 |
| **TXT / MD** | không có magic, đoán: UTF-8 hợp lệ & tỉ lệ ký tự in được cao | toàn file | tên file + header `Title:` (nếu có) | 1 |
| **HTML / XHTML / MHTML** | bắt đầu bằng `<` (sau khi bỏ BOM/khoảng trắng) | toàn file → `lol-html` | `<title>`, `<meta name="author">` | 1 |
| **FB2, `.fb2.zip`** | `<?xml` + root `<FictionBook` | `<body>` | `<description>/<title-info>` (title, author, genre, annotation, date, lang) + `<publish-info>` | 2 |
| **DOCX** | ZIP + entry `word/document.xml` | — | `docProps/core.xml` | 3 |
| **ODT** | ZIP + `mimetype` = `application/vnd.oasis.opendocument.text` | `content.xml` | `meta.xml` | 3 |
| **RTF** | `{\rtf` | — | `{\info …}` | 3 |
| **LIT** | `ITOLITLS` | — | — | 3 |
| **KFX (KF10)** | container magic `CONT` | — | — | 3 |
| **PDF / AZW4 (Print Replica = PDF bọc)** | `%PDF` | — | — | không index content |
| **DJVU** | `AT&TFORM` | — | — | không index content |
| **CBZ / CBR / CB7** | ZIP toàn ảnh | — | — | không index content |

> ⚠️ **Đính chính một niềm tin phổ biến:** MOBI/AZW/AZW3 **không giống EPUB ở tầng container**. EPUB là ZIP; MOBI/AZW3 là **PalmDB (PDB)** với `BOOKMOBI` header, body nén bằng **PalmDOC (LZ77 + Huffman)**. Cái "giống EPUB" là **payload bên trong là HTML** — nên giai đoạn `lol-html` của ta dùng lại được, còn giai đoạn đọc container phải viết riêng. PalmDOC giải nén chỉ ~150 dòng, đáng làm.
> ⚠️ `.azw4` **không phải** sách text — nó là PDF bọc trong container Amazon. Đừng cố index.
> ⚠️ số hiệu `.azw5/6/7/8` trong thực tế không nhất quán; **đừng phân biệt bằng số hiệu, hãy sniff `BOOKMOBI`**. File `.azw3` nào mà không có `BOOKMOBI` thì coi như không index được.

**Đã kiểm chứng trên thư viện thật** (`cargo run --release --bin inspect -- <file.epub>`):
1 EPUB KonoSuba → `detected: Epub`, **25 section** theo spine, metadata đầy đủ
(title/authors/language/series/series_index/identifiers), **405 KB text** từ file 2,2 MB.

#### 3.0.2 Quy tắc khi sniff xong
- Sniff ra format trong danh sách hỗ trợ → index.
- Sniff ra format **không** hỗ trợ (PDF/CBZ/DJVU/KFX…) → ghi `format` vào `BOOKS`, **không tạo posting**. Nếu `meta_only_unparseable = 1` thì vẫn trích metadata nếu đọc được (PDF title, tên file) để người dùng tìm thấy cuốn sách.
- Sniff **thất bại** (không khớp magic nào, không phải UTF-8/text) → bỏ qua, log `debug` level. **Không crash.**
- Sniff ra format **khác** với extension → **tin magic**, ghi log `warn` 1 lần/sách (giúp người dùng phát hiện thư viện lộn xộn).

### 3.1 Scan (chỉ đọc metadata, không mở nội dung)

```
1. duyệt source_dirs, bỏ qua theo §3.5.2 (thư mục loại trừ)
2. với mỗi entry:
     ├── là thư mục tên *.sdr / nằm trong exclude_dir_globs  → bỏ qua cả thư mục
     ├── file ẩn (bắt đầu bằng '.')                          → bỏ qua
     ├── extension ∈ exclude_extensions                      → bỏ qua (IBM AppleDouble, thumbnail…)
     ├── extension ∉ include_extensions                      → bỏ qua
     └── size ngoài [min_file_size, max_file_size]           → bỏ qua
3. sort theo mtime DESC (ưu tiên sách mới — §1.2)
4. với mỗi file còn lại:
     tra PATHS[path]
     ├── không có                                → NEW   : cấp id = hwm++, ghi PATHS
     ├── có, và (size, mtime) trùng BOOKS[path]  → SKIP  : không mở file (0 I/O)
     └── có, nhưng (size, mtime) khác            → UPDATE: cấp id mới, id cũ thành orphan
5. id còn trong BOOKS mà không thấy trên đĩa     → ORPHAN: xóa BOOKS + PATHS + METADATA, posting tự hủy
6. ghi scan_cursor sau mỗi ~200 file
```

**Bước 2 không mở file** — chỉ `read_dir` + `stat`. Đó là toàn bộ lý do tồn tại của `include_extensions`: nếu không có nó, ta phải mở 64 byte của **mọi** file trong `/documents` (kể cả ảnh, `.lua`, `.log`) mới biết cái nào index được.

Ghi `scan_cursor` và mỗi sách **1 transaction riêng** → mất điện tối đa mất 1 sách.

> **Không** dùng mtime của thư mục để bỏ qua scan. Trên vfat, bản copy qua USB có thể giữ nguyên mtime+size; chỉ `read_dir` (rẻ) mới phát hiện được set file thay đổi.

#### 3.1.1 Hai chế độ quét: `scope` và `full`

`config_hash` gọi `source_dirs` (nhiều thư mục) chỉ dùng cho **full scan**. Nhưng user hiếm khi
chờ index xong mới search — họ mở thư mục đang xem rồi gõ ngay. Hai chế độ:

| | `scan_scope(dir)` | `scan_full(config)` |
|---|---|---|
| Roots | **một** thư mục user đang mở | mọi `source_dirs` |
| Khi nào | user gõ search lần đầu | user bấm "index toàn bộ", hoặc lịch định kỳ |
| Thời gian | **vài giây** (chục cuốn) | **vài phút** (cả thư viện) |
| `ScanMode` | `Scope` | `Full` |

Hàm Rust: `scan_scope(engine, filter, dir)` và `scan_full(engine, config)`, cả hai đi qua
`scan_with_mode(engine, filter, roots, mode)`.

**Search cũng phải scope.** Cùng lý do: user đang ở một thư mục con mà tự nhiên query chạy trên
toàn bộ DB thì (a) kết quả lẫn sách họ không quan tâm, (b) lần đầu tiên DB rỗng nên chẳng có gì
để tìm. Query nhận thêm `ScopeFilter::new(dir)`; kết quả chỉ giữ sách có path nằm dưới `dir`.

**Bất biến quan trọng — scope không được xóa sách của scope khác.** Mỗi sách lưu thêm
`scope` = root đã phát hiện ra nó. Trong `scan_with_mode`, sách chỉ bị báo `missing` nếu
nằm dưới một root **thực sự đọc được trong lượt này** (`covered_prefixes`). Nếu không có
guard này thì kịch bản sau sẽ mất sách:

1. User scope-index `/documents/Books/A` (không nằm trong `source_dirs`) → 2 cuốn vào DB.
2. User chạy full scan với `source_dirs = [/documents/Books/B]`.
3. Không có guard → 2 cuốn trong A bị coi là "mất trên đĩa" → **xóa sạch**.

Cùng cơ chế cũng bảo vệ trường hợp rút thẻ SD: root không đọc được ⇒ không kết luận được gì ⇒
không có sách nào bị orphan.

> So sánh dùng `path == scope || path.starts_with(scope + "/")`, không bao giờ so sánh byte
> thuần, để `/docs` không khớp nhầm `/documents-extra`.

> `scanned_roots` trong `ScanPlan` ghi lại mọi root đã đi qua (đọc được hay không) để UI hiển
> thị "đã index những thư mục nào" và để tính `Coverage` (`Empty`/`Partial`/`Full`).

### 3.2 Trích xuất, Metadata & Tokenize

Thứ tự bắt buộc: **decode format → trích metadata → tokenize nội dung**. Metadata trích một lần, ghi `METADATA`, rồi đưa vào blob `METAIDX`.

1. **Decode container** theo format đã sniff (§3.0.1) → danh sách "section" có thứ tự đọc xác định:
   - EPUB: entry theo OPF `<spine>` (parse `META-INF/container.xml` → OPF → `<spine>`/`<manifest>`). Fallback nếu parse lỗi: sort theo tên entry — và đánh dấu `flags.partial`.
   - MOBI/AZW3: giải nén toàn bộ text records thành **một** section duy nhất, cắt theo dấu hiệu chương nếu có.
   - TXT/MD: **một** section = toàn file (decode encoding bằng `encoding_rs`, ưu tiên BOM → heuristic).
   - HTML/XHTML: một section.
   - FB2: `<body>` → mỗi `<section>` là một section.
2. **Trích metadata** bằng parser riêng cho từng format (OPF XML / PDB EXTH / FB2 XML / `<title>` / tên file). Ghi `METADATA[book_id]`. Nếu không trích được gì → `provenance = filename`, dùng tên file làm `title`.
3. **Tạo `METAIDX` blob** cho sách này: normalize (cùng normalizer §3.2 bước 4) rồi nối các field được bật trong `index_fields`, phân cách bằng `\n`. Blob này sẽ được ghi lại vào `METAIDX` khi session đóng.
4. Entry là `.xhtml`/`.html` → `lol-html` rewriter, **chỉ ghi text trong `<body>`**, ignore `<head>/<style>/<script>/<meta>/<!-- -->`.
5. **Làm sạch biên chunk:** buffer `pending` mang token dở qua giữa các chunk. Mọi ranh giới thẻ **flush** `pending` (tránh `foo<em>bar</em>` dính thành `foobar`).
6. **Normalize (thứ tự bắt buộc):**
   ```
   bytes → utf8 (BOM/entity đã do lol-html xử lý) → NFD → bỏ combining mark (U+0300..=U+036F)
        → lowercase → NFKC nhẹ (½, ², dấu câu kép)
   ```
   → `"Thám"` và `"tham"` cùng một key.
7. **CJK:** không có space → mỗi ký tự CJK là 1 token (unigram) và **thêm bigram** cho cặp ký tự liền nhau, cả hai mang position của ký tự đầu. "中文测试" → `中文@p, 文测@p+1, 测试@p+2` và `中@p, 文@p+1, 试@p+2, 测@p+3`. Cùng một bộ đếm position với từ Latin.
8. **Position** = bộ đếm tăng dần trong toàn bộ sách (liên tục qua các section theo thứ tự đọc), bắt đầu 0. **Metadata KHÔNG nằm trong không gian position** — nó nằm ở `METAIDX` (§2.3), nên position 0 luôn là token đầu tiên của nội dung. Điều này giữ cho logic checkpoint/snippet sạch và không phải xử lý trường hợp đặc biệt.
9. **Checkpoint:** mỗi `CHECKPOINT_INTERVAL = 2048` token, ghi lại cặp `(pos, byte_off_hiện_tại)` → `ENTRYMAP.checkpoints`. Đây là **thay thế** cho việc lưu `byte_off` ở từng posting.
10. **Cap mềm:** tối đa 100.000 pos cho mỗi (sách, term); vượt thì set `flags.partial` + `flags.truncated` trong `DICT`.
11. **Cap an toàn** (mỗi format): section ≤ 32MB giải nén, ≤ 2000 section, tổng ≤ 512MB.

### 3.3 Nén & ghi

- Sort term theo thứ tự byte trước khi ghi → insert tuyến tính vào B-tree của redb (nhanh hơn nhiều so với ngẫu nhiên).
- Posting chỉ còn 3 field: `doc_gap`, `pos_gap`, `entry_idx`.
- Append chunk mới với `chunk_idx = chunk_count`; cập nhật `DICT_DELTA` (df, tf, chunk_count).
- Merge `DICT_DELTA` → `DICT` khi session đóng.

### 3.4 Chiến lược nén tối đa (max compression)

Nén theo 6 lớp, xếp theo mức độ **tiết kiệm được bao nhiêu**, không phải theo mức độ hay ho:

#### L1 — Bỏ field tốn kém nhất: `byte_off` *(tiết kiệm ~15–20%)*
`byte_off` là giá trị tuyệt đối gần phân bố đều trong `[0, entry_len]` → entropy cao nhất trong posting. V3 **bỏ hẳn** khỏi posting, chỉ giữ `entry_idx` + `pos`. Snippet sau này dò checkpoint gần nhất, seek tới `byte_off` của checkpoint, rồi tokenize tiếp tối đa 2048 token (~10–20KB) để tìm vị trí chính xác. Chi phí snippet tăng vài ms — chỉ xảy ra cho ≤3 snippet/sách.

#### L1.1 DB không nhân bản text — chỉ giữ một dạng dẫn xuất

Câu hỏi thường gặp: *"DB lưu 2 bản, một real một bỏ dấu à?"* **Không.**

| | Lưu trong DB? | Ghi chú |
|---|---|---|
| Text thật của sách | **Không** | Nằm trong `.epub` trên đĩa, đó vẫn là nguồn duy nhất. Toàn thư viện đo được 348,7 MB — không nhân bản vào DB |
| Term đã fold + position + doc_id | **Có** (`POSTINGS`) | Đây **không phải** "bản text bỏ dấu" mà là cấu trúc khác: túm term riêng biệt kèm vị trí. Toàn thư viện đo được 102,9 MB |
| Metadata hiển thị (title, author…) | **Có** (`METADATA`) | Vài trăm byte/cuốn |

Tức DB chỉ chiếm ~103 MB cho cả chỉ mục, không phải ~452 MB.

#### L1.2 Position chính xác có vẻ thừa, nhưng đo được thì ngược lại

Câu hỏi hợp lý: *"nếu preview vẫn phải lục lại `.epub`, thì lưu position chi tiết
làm gì?"* — vì việc lục lại **không phải quét, nó là seek tới chỗ đã biết**.

Đo trên 120 cuốn thật (12,7M posting), `bin/postest`:

| Cách lưu | Kích thước | Cả thư viện | Phrase query "tham tu" |
|---|---|---|---|
| Chỉ doc_id + freq | 2,0 MB | ~10 MB | phải extract mọi sách ứng viên ≈ **9 giây** |
| Position làm nhám 1024 token | 6,1 MB | ~31 MB | quá nhiều báo giả |
| Position làm nhám 256 token | 8,4 MB | ~43 MB | quá nhiều báo giả |
| Position làm nhám 64 token | 11,3 MB | ~58 MB | 5903 ứng viên phải xác minh (**5,6 giả / 1 đúng**) |
| **Position chính xác** | 20,7 MB | **102,9 MB** | **896 đúng, 0 báo giả**, ~5ms |

Hàng "làm nhám 64 token" nhìn có vẻ hời (−45% dung lượng) nhưng sinh 6,6× số ứng
viên phải xác minh, mỗi cái ~10ms extract → query chậm thêm ~6 giây. Nó **tự phá**
ngân sách p95 < 50ms đã chọn ở §1.4. Đổi 45MB lấy 5ms là cái giá đáng.

> **Position và checkpoint không trùng nhau.** Position trả lời "token thứ mấy
> trong sách"; checkpoint trả lời "token thứ mấy nằm ở byte nào trong file". Cả hai
> đều cần: không có position thì phải quét cả cuốn, không có checkpoint thì position
> không dùng translation sang được vị trí đọc.

**`byte_off` trong checkpoint phải trỏ vào byte THÔ của entry trong ZIP, không phải vào text đã decode.**

Lý do: byte thô của `.epub` không bao giờ đổi; nếu file đổi thì `size`/`mtime` đổi nên ta index lại. Ngược lại, nếu tôi sửa một dòng config `lol-html`, bộ decode entity, hay thêm một bộ lọc — mọi offset kiểu "vào text đã decode" sẽ trỏ sai chỗ **mà không có lỗi nào**, chỉ là snippet hiện sai. Đó là lỗi khó debug nhất loại có thể có.

#### L2 — Chọn coder theo **tính đơn điệu** của từng cột *(tiết kiệm ~10–20% so với PForDelta)*
Đây là điểm mấu chốt: mỗi cột có hình dạng khác nhau, dùng một coder cho cả 4 cột là lãng phí.

| Cột | Hình dạng | Coder | Vì sao |
|---|---|---|---|
| `doc_gap` | đơn điệu, gap nhỏ | **Elias-Fano** | gần tối ưu cho dãy đơn điệu, overhead chỉ ~2 bit/phần tử, decode vét cạn |
| `pos_gap` | đơn điệu trong cùng doc, biên độ phụ thuộc doc | **Binary Interpolative Coding (BIC)** | tối ưu khi biết biên trên; thắng PForDelta rõ ở gap lớn và biến thiên (đúng với tail của phân bố Zipf) |
| `entry_idx` | gần như hằng số trong suốt một doc | **RLE** | chuyển thành run `(value, run_len)` → ~0.1 bit/posting |
| ~~`byte_off`~~ | — | **đã bỏ** (L1) | — |

Block = 128 posting. Mỗi block chọn coder theo `frac_bits` thực tế của dải; nếu một giá trị vượt thì đẩy vào danh sách exception. Nếu decode BIC phức tạp quá cho deadline, chốt phương án dự phòng **OptPFD** (tự chọn giữa PForDelta và VByte cho từng block) — ít nén hơn BIC vài % nhưng đơn giản hơn nhiều.

#### L3 — Lớp entropy trên đầu *(tiết kiệm ~10–15%)*
Sau L2, áp `zstd` lên toàn bộ block đã mã hóa. Dữ liệu đã entropy-coded vẫn nén thêm được vì PForDelta/Elias-Fano để lại nhiều bit dư. Dùng **trained dictionary** (train từ ~500 chunk mẫu) — chunk của ta nhỏ và đồng dạng hình dạng nên dictionary trained thường ăn thêm 5–10% so với zstd không train.

**Level: `zstd` level 3 — KHÔNG dùng level 19.** Đo trên 348,7MB text thật:

| level | kích thước | thời gian (host x86) |
|---|---|---|
| 3 | 100,9 MB (28,9% text) | 6,5 s |
| 19 | 68,4 MB (19,6% text) | **512 s** |

Level 19 chỉ nhỏ hơn level 3 đúng **32%** nhưng chậm gấp **~79×**. Trên Cortex-A9 (chậm hơn host ~10–20×) thì level 19 mất **1,5–3 giờ** cho một lần build toàn thư viện, trong khi level 3 chỉ **~2 phút**. Level 19 chỉ dùng được khi build offline trên host rồi copy sang Kindle — khai báo đường đó là tuỳ chọn, **không phải mặc định**.

Decompress nhanh ở mọi level (~1GB/s) và ta chỉ decode ~10KB mỗi chunk query, nên chọn level chỉ ảnh hưởng đường *ghi*, không ảnh hưởng ngân sách p95 < 50ms.

#### L4 — Dictionary gói gọn *(tiết kiệm ~30–50MB + nhanh hơn)*
Front coding trên dictionary đã sort: lưu `[prefix_len][suffix]` thay vì cả term → giảm ~50–60% kích thước term text. Gộp theo block 32 term để vẫn binary-search được. Sau đó zstd. Và đặt vào **một KV duy nhất** (`DICT`) — xem §2 để biết vì sao 1M KV sẽ mất 30–50MB chỉ cho overhead trang của redb. Kèm theo đó, prefix search từ "vài page-read" thành "binary search trong RAM".

#### L5 — Sự thật về redb (đã kiểm tra tài liệu, đừng đoán)
**redb 2.x không có nén native.** `Database::builder()` chỉ cung cấp `set_cache_size()`, `set_page_size()`, `set_repair_callback()`, `set_region_size()`. Value vào redb là byte thô, không qua bất kỳ lớp nén nào.

→ Hệ quả: **mọi lớp L1–L4 phải do ta làm trước khi ghi**, và nếu ai đó bỏ lớp nào thì index sẽ to gấp 2–5 lần mà không có warning nào từ redb. Đây cũng là lý do schema ta lưu `codec_version` trong `META`.

#### L6 — Thu hồi chỗ (reclaim)
- Posting mồ côi (sách đã xóa/đổi) nằm lại trong DB. Compact/rebuild toàn bộ khi mồ côi > 25% DB.
- redb dùng copy-on-write: rewrite cả chunk (chứ không sửa tại chỗ) → `fragmented_bytes` sẽ lớn dần. `compact()` định kỳ.

#### Tóm tắt ngân sách

| Thành phần | Kỹ thuật | ~bit/posting | 600M posting |
|---|---|---|---|
| `doc_gap` | Elias-Fano | 2.5 | ~190 MB |
| `pos_gap` | BIC | 5.5 | ~415 MB |
| `entry_idx` | RLE | 0.3 | ~23 MB |
| `byte_off` | **bỏ** → `ENTRYMAP` | 0 | ~5 MB |
| block header + exceptions | | 1.0 | ~75 MB |
| zstd-19 + trained dict | −15% | | −105 MB |
| **Postings** | | **~8.6** | **~600 MB** |
| Dictionary | front coding + zstd, 1 KV | | ~5 MB |
| `BOOKS`/`PATHS`/`ENTRYMAP`/`DICT` | | | ~20 MB |
| `METADATA` + `METAIDX` (6.000 cuốn) | blob metadata ~2MB nén | | ~15 MB |
| **TỔNG (điểm ước tính)** | | | **~640 MB** |

> ⚠️ Con số trong bảng là **ước tính** để chọn coder, không phải cam kết. Sai số có thể ±50% (corpus ít từ lặp sẽ tốt hơn, corpus nhiều tên riêng/code sẽ xấu hơn), nên §1.1 ghi **khoảng 500–700MB** thay vì một con số. Điểm cần định đoạt là §7.7 — đo thật trên 300 cuốn rồi hiệu chỉnh. Nếu đo ra xấu hơn nhiều, khâu nên tinh chỉ đầu tiên là `pos_gap` (chiếm 2/3 budget), kế đến là bật `index_depth = balanced`.

---

### 3.5 Cấu hình đuôi file & thư mục

Người dùng chỉnh từ UI KOReader → FFI `config_set()` → bảng `CONFIG`. Không có file cấu hình riêng, không hardcode đường dẫn trong Rust.

#### 3.5.1 Danh sách đuôi file mặc định

**Mặc định ĐƯỢC index** (có chữ để đọc):
```
epub  kepub  mobi  azw  azw3  txt  text  md  markdown
fb2  fb2.zip  html  htm  xhtml  xht  htm  shtml
```

**Mặc định BỊ LOẠI TRỪ** (rác, không phải sách):
```
lua  log  json  xml  opf  ncx  css  js  mjs  py  rs  sh  bash  toml  yaml  yml
ini  cfg  conf  bak  tmp  temp  part  partial  swp  old  orig  db  db3  sqlite3
sqlite  png  jpg  jpeg  gif  webp  bmp  ico  svg  mp3  mp4  mkv  woff  woff2  ttf
zip  gz  tgz  bz2  xz  7z  rar  tar  zst
pdf  djvu  djv  cbz  cbr  cb7  cbt  azw4  azw6  azw8  kfx  tpz
sdr  opf  toc  ncx  calibre  cover
```
> Ghi chú: `pdf/djvu/cbz` nằm trong danh sách loại trừ **về mặt content search** (§3.0.2), nhưng vẫn bị loại ở bước pre-filter. Nếu bật `meta_only_unparseable = 1` thì chúng được **đưa vào** `include_extensions` để vẫn index metadata.

**Quy tắc giải quyết xung đột:** `exclude_extensions` **thắng** `include_extensions`. Người dùng thêm `.azw3` vào include mà `.azw4` nằm trong exclude thì `.azw4` vẫn bị loại.

**Người dùng muốn thêm đuôi mới:** thêm vào `include_extensions` (vd `.azw4`, `.dat`, `.prc`). Nếu file thực chất là MOBI/EPUB/TXT thì magic sniff (§3.0) vẫn nhận ra đúng. Đây là điểm mạnh của việc tách pre-filter và sniff.

#### 3.5.2 Loại trừ thư mục

```
exclude_dir_globs (mặc định): *.sdr, .calibre*, .thumbnails, @eaDir, __MACOSX,
                              koreader, system, extensions, .Trash*, lost+found
```
Thư mục khớp glob → **bỏ qua cả cây con** (không đệ quy vào). Đây là tối ưu quan trọng: `<book>.sdr/` của KOReader chứa `metadata.epub.lua`, `custom.css`, `cover.jpg` — đều là rác so với ta.

Thêm nữa, bỏ qua mọi entry bắt đầu bằng `.` (trừ khi người dùng thêm glob `.*` vào include).

Nguồn quét mặc định: `/mnt/us/documents`. Người dùng có thể thêm `/mnt/us/books` hoặc một thư mục bất kỳ.

#### 3.5.3 Option nào phải rebuild index, option nào chỉ cần rescan

Đây là chỗ dễ gây lỗi âm thầm nhất — **đổi option nhạy cảm mà không rebuild = index sai, không ai biết.**

| Option | Ảnh hưởng | Hành động khi đổi |
|---|---|---|
| `fold_diacritics` | đổi token stream | **rebuild toàn bộ** (bump `config_hash`) |
| `cjk_bigrams` | đổi token stream | **rebuild** |
| `index_fields` | đổi nội dung blob metadata | **rebuild `METAIDX`** |
| `include_description` | đổi nội dung blob metadata | **rebuild `METAIDX`** |
| `index_depth` (§1.2) | đổi position | **rebuild** |
| `checkpoint_interval` | không đổi token, chỉ mật độ checkpoint | ghi lại ENTRYMAP ở lần index kế tiếp |
| `include_extensions` | đổi tập file quét | **rescan** (không rebuild) |
| `exclude_extensions` | đổi tập file quét | **rescan**; file bị bỏ → orphan tự hủy |
| `exclude_dir_globs` | đổi tập file quét | **rescan** |
| `source_dirs` | đổi tập file quét | **rescan** |
| `min/max_file_size` | đổi tập file quét | **rescan** |
| `meta_only_unparseable` | đổi tập file quét | **rescan** |
| `prefix_search_mode` | chỉ đổi cách query mở rộng term, không đổi token stream | **không cần rescan, không cần rebuild** — chỉ cần reload engine |

Nhóm trên chỉ cần rescan vì posting của sách cũ **không hề thay đổi** — đó là hệ quả trực tiếp của thiết kế "index lại = cấp `book_id` mới" (§0 #5).

#### 3.5.4 Cách tốt nhất để tránh rebuild oan
`config_hash` trong `META` chỉ được tính từ **nhóm rebuild** ở bảng trên. Nhóm rescan đổi thoải mái. Nếu ai đó lỡ thêm một option nhạy cảm mà quên đưa vào hash → đoạn này là nơi phải cập nhật, và §7 có test chống hồi quy cho việc này.

---

## 4. Truy vấn

### 4.1 Phrase query (nội dung sách)
1. Tokenize query bằng **đúng** normalizer ở §3.2 (đảm bảo tiền định) → nếu có CJK, sinh thêm bigram.
2. Dò `DICT` (+ `DICT_DELTA` nếu đang có) → `df` mỗi term. **Xếp theo df tăng dần** (hiếm nhất trước).
3. Với term hiếm nhất: duyệt lần lượt chunk; trong mỗi chunk decode từng block (chỉ ~64KB RAM/decode), thực hiện **positional intersect** với ràng buộc `pos[i+1] - pos[i] = gap_i` (`gap = 1` cho liền nhau — đúng vì stopword vẫn được index).
4. Binary-search chunk cho term còn lại bằng `first_book_id` ở đầu value, thay vì decode toàn bộ.
5. Posting trỏ tới `book_id` không còn trong `BOOKS` (mồ côi) → bỏ qua.

#### 4.1.1 Fallback prefix expansion — **bắt buộc, không phải tùy chọn**

**Vấn đề:** query `cha` chỉ khớp từ `cha`. Trong thư viện thật, người dùng gõ "cha", "sher", "nguy" với ý định là "chai", "sherlock", "nguyễn". Trên bàn phím PW3 (d-pad, gõ chậm, dễ mỏi) thì **gõ một phần của từ là hành vi chủ đạo**, không phải edge case. Chế độ exact-only sẽ làm search thất bại thường xuyên.

**Giải pháp — quy tắc "không có trong dict thì mở rộng thành prefix":**

```
với mỗi term t của query:
    nếu t ∈ DICT                    → term_set(t) = { t }              # từ đầy đủ, KHÔNG mở rộng
    nếu t ∉ DICT                    → term_set(t) = prefix_scan(t)     # mở rộng
```

Vì sao chỉ mở rộng khi `t` **không** có trong dict: nếu `t` là từ tồn tại thật (vd `cha`, `the`, `nó`) thì người dùng có ý định gõ đúng từ đó — mở rộng sẽ làm kết quả bắt ngẫu nhiên. Nếu `t` không phải từ nào trong cả thư viện (`sher`, `nguy`) thì gần như chắc chắn là từ chưa gõ xong. Quy tắc này cho độ chính xác cao với chi phí gần bằng không.

**Chi tiết thuật toán:**
1. `prefix_scan(t)` = range scan `DICT` trong `[t, successor(t))`, lấy theo `df` giảm dần, chặn bởi cả hai mức: **≤ 64 term** và **tổng `df` ≤ 100.000**. Vượt cả hai → trả về rỗng, set `status = too_broad` (báo UI "quá nhiều kết quả, gõ thêm ký tự").
2. Với mỗi term t, **gộp postings của cả `term_set(t)` thành một stream duy nhất** (`union_postings`). Stream vẫn giữ thứ tự `(book_id, pos)` — xem lưu ý bên dưới.
3. Positional intersect giữ nguyên: `pos[i+1] - pos[i] = gap_i`. Vậy query `cha chai` với `cha` → `{chai, chan}` sẽ khớp đúng cả "chai chai" lẫn "chan chai". **Không phải làm cặp từng term (không phải |T1|×|T2|).
4. Nếu `term_set(t)` có > 1 term → đánh dấu match là `fuzzy`. **Phải hiển thị cho người dùng biết** (vd gạch chân từ đã mở rộng trong ô tìm kiếm), nếu không họ sẽ không hiểu tại sao gõ `cha` lại ra sách không có chữ "cha".
5. Ranking: match trên term được mở rộng bị giảm hệ số `0.7` so với exact.

**Setting người dùng:** `prefix_search_mode` = `auto` (mặc định, quy tắc trên) | `exact` (chỉ khớp term đầy đủ) | `prefix` (luôn mở rộng mọi term, kể cả từ đã có trong dict — cho người dùng muốn recall tối đa).

**Vì sao `union_postings` vẫn đúng thứ tự:** trong `DICT` các term đã sort theo byte, nên các term bắt đầu bằng cùng prefix **liền nhau trong B-tree của redb**. `union_postings` merge các chunk theo thứ tự `(term, chunk_idx)` → trong phạm vi một term thì vẫn sort đúng theo `(book_id, pos)`. Intersect hai pointer vẫn chạy tuyến tính.

### 4.2 Tìm theo metadata (title / author / series / tag)
Chạy **song song và độc lập** với §4.1, không dùng postings:

1. Nạp `METAIDX` (2–3MB nén → ~5MB RAM, LRU 1, chỉ nạp 1 lần mỗi đợt query).
2. Chuẩn hóa query bằng đúng normalizer §3.2 → `q`.
3. Với **mỗi** blob sách (6000 blob × ~300 byte = 1.8MB): `memmem(blob, q)`. SIMD memchr quét 1.8MB ≈ < 3 ms trên Cortex-A9.
4. Nếu `q` không khớp blob nào nhưng khớp ở dạng token → cũng thử khớp theo từng token của `q` (để "nguyễn nhật ánh" tìm được "Nguyễn Nhật Ánh" khi blob chứa đúng chuỗi đó).
5. Trả về `(book_id, field_mask)` cho biết khớp ở field nào (`title`, `authors`, `series`, `tags`…).

**Vì sao không dùng inverted index cho metadata:** postings chỉ khớp được **cả token** hoặc prefix, còn người dùng gõ `nguy` chắc chắn muốn ra `Nguyễn`, gõ `imov` muốn ra `Asimov`. Khớp chuỗi con trên 1.8MB vừa nhanh vừa đủ — không cần index.

### 4.3 Prefix / gợi ý (autocomplete)
- Với **từ trong nội dung**: binary search trong `DICT` (đã nạp RAM, §3.4 L4) từ prefix đến successor (vd `"sher"` → `"shes"`). Giới hạn 200 term đầu, gộp posting.
- Với **metadata**: quét 6000 blob trong `METAIDX` bằng prefix của từng field. Cùng chi phí ~3 ms.
- Gộp cả hai nguồn gợi ý, ưu tiên metadata hit (tên sách/tác giả) lên trước.
- **Dùng lại đúng hàm `prefix_scan()` của §4.1.1** — không viết hai lần. Gợi ý là preview của việc user sẽ bấm Search, nên hai bước phải cho kết quả nhất quán.

### 4.4 Ranking (gộp 2 nguồn)
```
score(book) = w_meta × meta_hits × idf_meta     # match title/author — tín hiệu mạnh nhất
            + Σ_term  tf_term_in_book × idf_term  # match nội dung
```
`w_meta` mặc định cao (~3×) vì người dùng gõ tên sách/tác giả thì ý định gần như chắc chắn là muốn cuốn đó, dù nội dung có thể cũng chứa cụm đó. Giảm dần, group theo sách, mỗi sách giữ tối đa 3 kết quả.

### 4.5 Snippet & mở sách
1. **Match nội dung:** `ENTRYMAP[(book_id, entry_idx)]` → `entry_name` + `checkpoints`.
   - Tìm checkpoint gần nhất **≤** `pos` (binary search trong mảng đã decode).
   - Mở file, seek `byte_off` của checkpoint, decode tiếp bằng `lol-html`, đếm token tới `pos` (≤ 2048 token), lấy ~700 byte quanh match → highlight.
   - LRU cache 2 file gần nhất (tránh mở lại cùng một sách khi user lướt kết quả).
2. **Match metadata:** không có snippet text — trả **thẻ metadata** (title, authors, series, publisher, năm, tags) từ bảng `METADATA`. Đây là điều người dùng mong đợi khi tìm theo tên sách.
3. **Lazy** khi user *chạm* kết quả: đếm element đến offset đó → dựng `Locator` (xpointer) cho KOReader nhảy đúng trang. Vài ms, 1 lần/kết quả được chọn.

---

## 5. Tích hợp KOReader

| Hạng mục | Quyết định |
|---|---|
| Artifact | `crate-type = ["cdylib"]` → `libturbosearch.so`, chỉ export hàm `extern "C"` + opaque handle (`Box::into_raw`) |
| Cross-compile | target `armv7-unknown-linux-gnueabihf`, **cùng toolchain/glibc với koreader-base** (hard-float, NEON). Không hard-code `-C target-cpu=cortex-a9` để còn chạy máy khác |
| Lua side | `ffi.cdef` + `ffi.load("/mnt/us/koreader/search/libturbosearch.so")` |
| Threading | Index chạy trong background task của KOReader; FFI expose `progress()`. **Không bao giờ block UI thread** |
| Vị trí DB | `/mnt/us/koreader/search/turbosearch.redb` |
| Crash safety | redb có WAL/ACID → mất điện không hỏng DB, chỉ mất tối đa 1 sách (§3.1) |
| ABI check | `so` kiểm tra `schema_version` lúc load; lệch → trả lỗi "rebuild needed", không crash |

Crate:
```
redb 2.x · zip 2.x · flate2 (miniz_oxide — thuần Rust, không cần zlib C)
lol-html 1.x · unicode-normalization · rustix (statvfs) · libc

Đa định dạng & metadata:
  quick-xml         — parse OPF (EPUB), FB2 <description>, META-INF/container.xml. Nhẹ, thuần Rust.
  encoding_rs       — TXT/MD/HTML: BOM sniff + heuristic cho UTF-8/UTF-16/cp1252/Shift-JIS/GBK.
  memchr            — memmem cho METAIDX (§4.2), và nhanh hơn naive scan nội bộ indexer.
  (tùy chọn) ebook-rs 0.16.x — only for long-tail formats, feature-gated `--features longtail`.
       LD flags: PDF (pdf_oxide), DOCX, RTF, ODT, LIT, KFX.
       Pure Rust, magic-byte detection, có metadata() + sections(). Nhưng chỉ mới 0.16,
       phụ thuộc nặng (zlib-rs, regex, whatlang, simdutf8, serde_json…) và 45% doc coverage.
       → KHÔNG dùng cho EPUB/TXT/MOBI/FB2 (4 format chiếm >95% thư viện Kindle, tự làm được).
       Chỉ bật khi user thực sự có file đuôi lạ và tự chọn longtail_formats = 1.
```

PalmDOC giải nén (~150 dòng, tự viết): xem §3.0.2. Không có crate ổn định, và đây là phần lõi của MOBI/AZW3 nên phải tự chắc tay.

Tự viết in-repo (không có crate đủ tốt): Elias-Fano, BIC, RLE, front coding, PForDelta dự phòng. Tổng ~600 dòng + property test.

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
longtail = ["ebook-rs"]     # tắt mặc định: nhị phân nhỏ, ít phụ thuộc hơn
```

---

## 6. RAM / I/O trên PW3

- **`METAIDX` nạp RAM khi có query metadata**: ~2–3MB nén → ~5–6MB giải nén. LRU 1, giải phóng được khi idle.
- `DICT` nạp RAM ~10–15MB resident (LRU 1). Khoản RAM lớn nhất.
- **Cả hai nên dùng chung một budget** `query_arena` ~24MB; nếu query không cần metadata thì không nạp `METAIDX`.
- Accumulator mỗi sách (hashmap term → Vec<posting>) ~5–15MB cho sách thường; sách outlier > 200MB text → **flush theo chunk thay vì giữ hết**.
- redb `set_cache_size(8..=16 MB)` — **mặc định của redb là 1GiB**, trên máy 512MB RAM thì đây là lỗi nghiêm trọng nếu quên.
- I/O: phase scan chỉ `stat` → rẻ. I/O nặng ở lần index đầu và snippet (cold read 1 entry ZIP).
- Buffer zstd giữ cố định ~64KB, không cấp phát lại mỗi chunk.
- **`METAIDX` rewrite 1 lần/session** = nén ~5MB → ~1ms CPU, không đáng lo. Nhưng **đừng** rewrite nó sau mỗi sách.

---

## 7. Kiểm thử

1. **Property test** mỗi coder: random `Vec<u32>` → encode → decode → bằng nhau. Elias-Fano, BIC, RLE, PForDelta, front coding, mỗi cái 1 triệu case trên host.
2. **Compression benchmark** trên host: đo bit/posting thực tế cho từng coder với dữ liệu Zipf nhân tạo + một corpus tiếng Anh/Việt/Trung thật → chọn coder bằng số đo, không bằng cảm tính.
3. **Golden test** tokenizer: 20 file HTML/EPUB mẫu (entity, comment, CDATA, `>` trong attribute, UTF-16, CJK, tiếng Việt) → snapshot term + position.
4. **So sánh** với reference `html5ever`/`lol-html` output trên cùng input.
5. **Round-trip** index → query cho cụm từ tiếng Việt có dấu/không dấu, CJK, cụm 4–5 từ, và cụm chứa stopword.
6. **Resume**: kill giữa chừng index → chạy lại → DB hợp lệ, không mất sách đã xong.
7. **Benchmark trên máy thật** 300 cuốn: đo p50/p95 query, thời gian index, **DB size thực tế** → hiệu chỉnh bảng §3.4. Đây là gate của giai đoạn 4.
8. **Format sniff test** (v4): 1 file mẫu/format × mỗi magic trong bảng §3.0.1 → assert enum đúng. Test thêm: file đổi tên sai đuôi, file rỗng, file 3 byte, file zip bomb, file trùng magic nhưng hỏng giữa chừng.
9. **Metadata test** (v4): mỗi format × metadata đúng field; sách thiếu metadata → `provenance = filename`; description > 4KB → `desc_truncated`; ký tự tiếng Việt/CJK trong title giữ nguyên dấu.
10. **Metadata search test** (v4): `nguy` → `Nguyễn`, `imov` → `Asimov`, `nhật ánh` → `Nguyễn Nhật Ánh`, tên file `harry potter` → file `Harry.Potter...txt`.
11. **Config test** (v4): sửa nhóm "rebuild" → assert `config_hash` đổi; sửa nhóm "rescan" → assert `config_hash` **không** đổi và sách mới vẫn được index. Chống hồi quy cho §3.5.3.
12. **Prefix expansion test** (v4): index corpus chứa "chai", "chair", "chaos" nhưng **không** chứa "cha" → assert query `cha` ra cả ba, và match được đánh dấu `fuzzy`. Index corpus có chứa "cha" → assert query `cha` **không** mở rộng (exact-only). Query `cha chai` với `cha`→`{chai,chan}` → assert khớp cả "chai chai" và "chan chai" theo adjacency. Query prefix quá rộng (`a`) → assert `status = too_broad` chứ không treo.

---

## 8. Không hỗ trợ (ghi rõ để user không bối rối)

- **KFX, CBZ, DJVU, PDF, DOCX**: không index → kết quả search sẽ không bao gồm. Không báo lỗi.
- **MOBI/AZW3**: phase 2 (cần PalmDOC + HTML Huffdic decompression).
- EPUB hỏng / entry khổng lồ: skip sách đó, ghi log, **không crash**.
- Cap an toàn: mỗi entry ≤ 32MB giải nén, ≤ 2000 entry/sách, tổng ≤ 512MB (chống zip bomb).
- Giới hạn biết trước và chấp nhận: entity bị cắt ngang giữa 2 chunk (`&am` + `p;`) có thể tạo sai 1 token rất hiếm.
- Snippet sau L1 chậm hơn vài ms (phải quét lại ≤2048 token) — đã tính vào ngân sách ở §1.4.

**Không hỗ trợ (ghi rõ để user không bối rối):**
- **PDF / AZW4 (Print Replica) / DJVU / CBZ / CBR**: không index nội dung. User vẫn thấy sách trong thư viện KOReader bình thường; chỉ là tìm kiếm toàn văn không ra chúng. Nếu bật `meta_only_unparseable` thì vẫn tìm được theo tên file.
- **KFX (KF10)**: container `CONT`/Ion độc quyền của Amazon — **không index trong phase 1–2**. Rất nhiều sách mua trên Amazon là KFX, nên đây là hạn chế lớn nhất; đã ghi vào lộ trình phase 3.
- **AZW5/6/7/8**: đặt tên không nhất quán trong thực tế. Xử lý bằng magic sniff (§3.0.1), không đoán theo số hiệu.
- **LIT / RTF / DOCX / ODT**: phase 3, hoặc opt-in qua `longtail_formats`.
- **Sách có DRM (Kindle/Adobe)**: không index. Không crack DRM — ta chỉ đọc được file đã giải mã sẵn.
- **CBZ/CBR**: chỉ có ảnh, không có text để index. Không chạy OCR (quá nặng cho PW3).
- **Metadata sidecar của KOReader** (`metadata.epub.lua`): phase 3, xem §2.3.
- **Text trong ảnh / scan**: không index (không OCR).

---

## 9. Lộ trình

| Giai đoạn | Nội dung | Định nghĩa xong | Trạng thái |
|---|---|---|---|
| 1 | Cargo + module layout + 4 coder (Ef/BIC/RLE/PForDelta) + property test | Round-trip pass mỗi coder, benchmark bit/posting | ✅ **xong** — 52 test, 0 warning |
| 1b | Real-corpus measurement (`bin/measure`) | Số đo toàn thư viện, không suy rọi | ✅ **xong** — 614 EPUB: 1,36GB→348,7MB text→index full 102,9MB |
| 2 | redb schema + `pack` + SCAN + BOOKS/PATHS/META/CONFIG + allocate id | Detect new/update/delete đúng; id không tái sử dụng; resume cursor | ✅ **xong** — 48 integration test, 3 bug thật bị bắt |
| 3 | **Format sniff + decoder TXT/HTML/EPUB** (magic byte + OPF spine + lol-html) | Golden test §7.3 + §7.8 pass; chạy thật trên thư viện | ✅ **xong** — 127 test; 25 section/sách, 405KB text từ 1 EPUB |
| 4 | **Metadata: OPF + FB2 + filename + METAIDX blob + provenance** | §7.9 + §7.10 pass — tìm được sách theo tên/tác giả | ✅ **xong** — 9 integration test; **43/43** cuốn có sidecar |
| 5 | POSTINGS + DICT/DICT_DELTA + chunk append | Kiểm tra DB size thật trên toàn thư viện vs bảng §1.1.1 | ⬜ |
| 6 | **Decoder MOBI/AZW/AZW3** (PalmDB + PalmDOC + EXTH) | Decode đúng text + metadata của 5 file mẫu trong thư viện | ⬜ |
| 7 | Query: phrase + prefix + rank + snippet + metadata match | p95 < 50ms trên máy thật | ⬜ |
| 8 | FFI + build armhf + tích hợp LuaJIT | Chạy trên Kindle PW3 thật | ⬜ |
| 9 | Budget guard, compact, UI tiến độ | Release | ⬜ |
| 10 | Long-tail: KFX, sidecar KOReader, `longtail` feature | Optional | ⬜ |

---
---

# PHỤ LỤC — ĐẶC TẢ v1 (gốc, giữ để lịch sử)

> Bản dưới đây là nội dung `note.md` trước khi nâng cấp. **Không còn là hướng dẫn hiện hành** — các điểm lỗi/thay đổi đã ghi ở §0.

```
check file change = inode + mtime + size
check dir change = mtime
```

## ĐẶC TẢ KỸ THUẬT: EMBEDDED FULL-TEXT SEARCH ENGINE TRÊN RUST CHO KOREADER

### 1. Môi trường & Ràng buộc Phần cứng

* Thiết bị mục tiêu: Kindle Paperwhite 3 (PW3) – CPU đơn nhân ARM, RAM hạn chế (512MB - 1GB), tốc độ đọc/ghi ổ cứng (I/O) chậm.
* Ngôn ngữ: Rust (Cross-compile sang kiến trúc ARM).
* Database: redb (Embedded Key-Value Store, thuần Rust, dạng Single-file).
* Mục tiêu hiệu năng: Tìm kiếm cụm từ/cả câu (Phrase Query) thời gian thực (< 50ms). Dung lượng DB chiếm dưới 3% dung lượng thư viện (Ước tính ~50MB cho 2.5GB sách).
* Chiến lược: Không chạy daemon giám sát file ngầm (watch fs), chỉ quét thủ công khi có yêu cầu (On-demand Incremental Scan). Chấp nhận một chút dương tính giả (false positive) từ mã nguồn HTML thô để đổi lấy siêu tốc độ.

### 2. Kiến trúc Database (redb)

**Bảng A: BOOKS_TABLE (Quản lý Danh mục Sách)**
* Key: book_id: u16 (Định danh duy nhất, tiết kiệm dung lượng, tối đa 65,535 cuốn).
* Value: Cấu trúc nén dạng Byte chứa: [path: String] + [inode: u64] + [mtime: i64] + [size: u64].
* Mục đích: Kiểm tra thay đổi file (Change Detection) siêu tốc bằng Metadata hệ thống mà không cần đọc nội dung file.

**Bảng B: INVERTED_INDEX_TABLE (Chỉ mục Đảo ngược)**
* Key: word: String (Từ khóa viết thường - lowercase).
* Value: Mảng Byte đã được mã hóa nén: CompressedPostingsList.
* Cấu trúc dữ liệu sau khi giải nén (Logic): Vec<(book_id: u16, positions: Vec<u32>)>.

### 3. Quy trình Xử lý dữ liệu (Pipeline)

#### Bước 1: Quét tăng trưởng siêu tốc (Incremental Scan)
1. Đọc mtime của thư mục sách chính. Nếu khớp với cấu hình cũ, bỏ qua toàn bộ việc quét file.
2. Nếu thư mục có thay đổi, lặp qua các file (std::fs::read_dir), lấy nhanh bộ ba metadata (Inode, Mtime, Size).
3. So sánh với BOOKS_TABLE: trùng khớp → bỏ qua file; thay đổi/mới → cần index; mất trên đĩa → xóa dữ liệu cũ.

#### Bước 2: Tokenizer thô & Trích xuất Vị trí (Positioning)
1. Dùng luồng đọc trực tiếp từ bộ nhớ (Memory streaming) qua crate zip, không ghi file ra đĩa flash.
2. Sử dụng State Machine tối giản: `InTag` (bỏ qua từ `<` đến `>`), `InText` (cắt từ, lowercase).
3. Gán chỉ số bộ đếm tăng dần (0, 1, 2, 3...) cho mỗi từ hợp lệ. Lọc bỏ stopwords.

#### Bước 3: Nén dữ liệu vị trí (Delta Encoding + VByte)
1. Delta Encoding: [100, 105, 120] → [100, 5, 15].
2. Variable Byte (VByte / Varint): số < 128 chỉ tốn 1 byte.

### 4. Thuật toán Tìm kiếm Cụm từ / Cả câu (Phrase Query)
1. Phân rã chuỗi: tách câu tìm kiếm thành token đơn lẻ.
2. Truy vấn RAM: trích xuất postings list 2 từ, giải nén Delta + VByte.
3. Positional Intersect two-pointer O(N+M): điều kiện `Position(từ_2) - Position(từ_1) = 1`.
4. Hiển thị kết quả: trả về book_id và vị trí byte/từ, `seek()` để trích snippet.

### 5. Các Thư viện (Crates) Khuyên dùng
* redb, zip, bstr hoặc regex

### 3.1 Bổ sung Bộ trích xuất Text sạch (HTML Parsing Strategy)
* Thư viện khuyến nghị: `lol-html` (Dạng Stream Tokenizer, tuyệt đối không dùng thư viện dựng DOM như `scraper`).
1. Chỉ trích xuất nội dung văn bản (Text content) nằm bên trong thẻ `<body>`.
2. Bỏ qua hoàn toàn (Ignore) toàn bộ thẻ và nội dung con bên trong các thẻ: `<head>`, `<style>`, `<script>`, `<meta>`.
3. Tự động giải mã (Decode) các HTML Entities (ví dụ: biến `&nbsp;` thành khoảng trắng, `&amp;` thành `&`) trước khi đưa vào bộ đếm Vị trí (Tokenizer).
4. Sau khi trích xuất chuỗi Text sạch, mới tiến hành gán chỉ số Position tăng dần (0, 1, 2, 3...) cho từng từ.
