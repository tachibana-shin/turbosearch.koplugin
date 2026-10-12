//! Term normalization and tokenization.
//!
//! Implements `note.md` section 3.2, steps 4 to 10. The pipeline is:
//!
//! ```text
//! bytes -> utf8
//!       -> NFD -> drop combining marks   (so "Thám" and "tham" collapse)
//!       -> lowercase
//!       -> NFKC (light)                  (so "½" and "1⁄2" collapse)
//!       -> split on anything not alphanumeric
//!       -> CJK: one token per character plus a bigram per adjacent pair,
//!         both carrying the position of the first character
//! ```
//!
//! Whitespace, tabs, carriage returns and linefeeds are all separators, so none
//! of them can ever end up inside a term.
//!
//! Positions are a monotone counter over the whole book, continuous across
//! sections, which is what makes the phrase-intersection in `note.md`
//! section 4.1 possible.

use unicode_normalization::UnicodeNormalization;

/// One indexed term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// Normalized term bytes, always valid UTF-8.
    pub term: Vec<u8>,
    /// Monotone position within the book.
    pub position: u32,
}

/// Normalizes text for comparison and indexing.
///
/// Order matters: NFD first so combining marks can be dropped, then lowercase,
/// then NFKC. Doing NFKC first would recompose some sequences and leave the
/// marks undetectable.
pub fn normalize(text: &str) -> String {
    OffsetMap::new(text).normalized
}

/// Maps letters that have no combining form onto their plain equivalent.
///
/// Only Vietnamese contributes: `đ`/`Đ` are the sole Latin-script letters whose
/// accent is part of the code point rather than a separate mark.
fn fold_letter(ch: char) -> char {
    match ch {
        '\u{0110}' | '\u{0111}' => 'd',
        _ => ch,
    }
}

/// The combining diacritical marks block.
///
/// Covers Vietnamese (U+0323 dot below, U+0302 circumflex, U+0303 tilde), most of
/// Latin Extended Additional, and the marks used by Greek and Cyrillic.
const COMBINING_MARKS: std::ops::RangeInclusive<u32> = 0x0300..=0x036F;

/// True for combining diacritical marks.
fn is_combining_mark(ch: char) -> bool {
    COMBINING_MARKS.contains(&(ch as u32))
}

/// True for scripts that do not separate words with spaces.
///
/// Chinese and Japanese would otherwise tokenize as one enormous "word" per
/// sentence, so each character becomes a token and each adjacent pair a bigram.
/// Hangul is excluded: Korean spaces its words.
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF       // Hiragana and Katakana
        | 0x3400..=0x4DBF     // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF     // CJK Unified Ideographs
        | 0xF900..=0xFAFF     // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F   // Extensions B..F and compatibility supplement
    )
}

/// True when `ch` can be part of a term.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

/// Feeds text through the normalizer and reports every term it contains.
///
/// Returns the position to pass for the next chunk, so a caller indexing a book
/// section by section keeps one continuous counter.
///
/// The callback receives borrowed term bytes and their position, so indexing a
/// whole book allocates nothing per token.
/// Feeds text through the normalizer and reports every term it contains.
///
/// Returns the position to pass for the next chunk, so a caller indexing a book
/// section by section keeps one continuous counter.
///
/// The callback receives borrowed term bytes and their position, so indexing a
/// whole book allocates nothing per token.
pub fn for_each_token(
    text: &str,
    start_position: u32,
    mut sink: impl FnMut(&[u8], u32),
) -> u32 {
    for_each_token_spanned(text, start_position, |term, position, _, _| {
        sink(term, position)
    })
}

/// Same walk as [`for_each_token`], but also reports where each term came from.
///
/// `start` and `end` are byte offsets into `text`, i.e. into the **original**
/// text with its capitalisation and diacritics intact. That is what makes a
/// result preview possible: normalizing is lossy, so the only way to show the
/// user "Thám tử" after they searched for "tham tu" is to keep a way back to the
/// bytes that produced the term.
///
/// Spans are exact for every character that survives normalization one-to-one,
/// which covers all ordinary prose. The exceptions are compatibility expansions
/// such as `½`, where several output bytes map back to one input byte, so the
/// reported span covers the whole source character rather than the single digit
/// that ended up in the term.
pub fn for_each_token_spanned(
    text: &str,
    start_position: u32,
    mut sink: impl FnMut(&[u8], u32, usize, usize),
) -> u32 {
    let mapped = OffsetMap::new(text);
    let normalized = mapped.normalized.as_str();
    let mut position = start_position;

    // A run of non-CJK word characters, flushed on any separator or when the
    // run switches between letters and digits.
    let mut run: Vec<u8> = Vec::new();
    let mut run_is_digits = false;
    let mut run_start = 0usize;
    // The previous CJK character, its normalized offset and its position, kept
    // so the next CJK character can form a bigram with it.
    let mut prev_cjk: Option<(char, usize, u32)> = None;

    for (offset, ch) in normalized.char_indices() {
        if is_word_char(ch) {
            if is_cjk(ch) {
                flush_run_spanned(&mut run, &mut run_start, &mut position, &mapped, &mut sink);
                let this_pos = position;
                let mut buf = [0u8; 4];
                let (start, end) = mapped.span(offset, offset + ch.len_utf8());
                sink(ch.encode_utf8(&mut buf).as_bytes(), this_pos, start, end);
                if let Some((prev, prev_offset, prev_pos)) = prev_cjk.take() {
                    // The bigram spans both characters, so start at the first.
                    let (bstart, bend) = mapped.span(prev_offset, offset + ch.len_utf8());
                    let mut pair = [0u8; 8];
                    let mut len = prev.encode_utf8(&mut pair[..]).len();
                    len += ch.encode_utf8(&mut pair[len..]).len();
                    sink(&pair[..len], prev_pos, bstart, bend);
                }
                prev_cjk = Some((ch, offset, this_pos));
                position += 1;
            } else {
                // A separator between two CJK runs breaks adjacency.
                prev_cjk = None;
                // "Chapter12" splits into "chapter" and "12": someone searching
                // for a chapter number must not have to type the whole word.
                let is_digits = ch.is_numeric();
                if !run.is_empty() && is_digits != run_is_digits {
                    flush_run_spanned(&mut run, &mut run_start, &mut position, &mapped, &mut sink);
                }
                if run.is_empty() {
                    run_start = offset;
                }
                run_is_digits = is_digits;
                let mut buf = [0u8; 4];
                run.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        } else {
            flush_run_spanned(&mut run, &mut run_start, &mut position, &mapped, &mut sink);
            prev_cjk = None;
        }
    }
    // Whatever is left ends the book, so it must be emitted.
    flush_run_spanned(&mut run, &mut run_start, &mut position, &mapped, &mut sink);
    let _ = run_is_digits;

    position
}

fn flush_run_spanned(
    run: &mut Vec<u8>,
    run_start: &mut usize,
    position: &mut u32,
    mapped: &OffsetMap,
    sink: &mut impl FnMut(&[u8], u32, usize, usize),
) {
    if run.is_empty() {
        return;
    }
    let (start, end) = mapped.span(*run_start, *run_start + run.len());
    sink(run, *position, start, end);
    *position += 1;
    run.clear();
}

/// Normalized text plus a byte map back to the original.
///
/// The map exists because normalization is lossy and irreversible: once "Thám"
/// has become "tham" there is no route back to the "á". Keeping the map is what
/// lets the query path show the user their own words.
struct OffsetMap<'a> {
    source: &'a str,
    normalized: String,
    /// One entry per byte of `normalized`, holding the source byte it came from.
    /// A trailing entry covers the end of the final character.
    map: Vec<usize>,
}

impl<'a> OffsetMap<'a> {
    fn new(text: &'a str) -> Self {
        let mut normalized = String::with_capacity(text.len());
        let mut map: Vec<usize> = Vec::with_capacity(text.len() + 1);
        let mut src = 0usize;

        // Group by ORIGINAL character: every byte the character produces, and
        // every mark it drops, is attributed to the character's own start
        // offset. Advancing inside a character would produce a span that lands
        // in the middle of it, which no slicing operation can use.
        for raw in text.chars() {
            let char_start = src;
            src += raw.len_utf8();

            // Hangul syllables are exempt from NFD. They decompose into
            // conjoining jamo, which are not combining marks, so an NFD pass
            // would tear "한국어" into jamo and never recompose it.
            if is_hangul_syllable(raw) {
                push_normalized(&mut normalized, &mut map, &raw.to_string(), char_start);
                continue;
            }

            // NFD per character, so a precomposed "á" can give up its mark.
            for decomposed in raw.nfd() {
                if is_combining_mark(decomposed) {
                    continue;
                }
                // `đ` folds to `d`: the stroke is part of the code point, not a
                // separate mark.
                let decomposed = fold_letter(decomposed);
                for lower in decomposed.to_lowercase() {
                    for composed in lower.nfkc() {
                        push_normalized(
                            &mut normalized,
                            &mut map,
                            composed.encode_utf8(&mut [0u8; 4]),
                            char_start,
                        );
                    }
                }
            }
        }
        map.push(src);

        Self {
            source: text,
            normalized,
            map,
        }
    }

    /// Source byte range for a normalized byte range.
    fn span(&self, norm_start: usize, norm_end: usize) -> (usize, usize) {
        let start = self.map.get(norm_start).copied().unwrap_or(0);
        // The end is exclusive, so the entry AT `norm_end` is the byte just past
        // the final character of the range.
        let end = self.map.get(norm_end).copied().unwrap_or(self.source.len());
        (start, end)
    }
}

/// True for a precomposed Hangul syllable.
///
/// NFD splits these into conjoining jamo, and NFKC applied per character cannot
/// recompose them, so they are left alone and stay one token per word.
fn is_hangul_syllable(ch: char) -> bool {
    matches!(ch as u32, 0xAC00..=0xD7A3)
}

fn push_normalized(normalized: &mut String, map: &mut Vec<usize>, s: &str, src: usize) {
    // `push_str`, not `push(byte as char)`: casting a byte to a char would
    // re-encode anything above 0x7F as multiple UTF-8 bytes.
    normalized.push_str(s);
    map.extend(s.bytes().map(|_| src));
}

/// Convenience wrapper collecting every token.
pub fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    for_each_token(text, 0, |term, position| {
        out.push(Token {
            term: term.to_vec(),
            position,
        })
    });
    out
}

/// True when the term contains no characters that need escaping further.
///
/// Callers storing terms as raw bytes can use this to confirm the normalizer
/// never produces a term still holding a combining mark.
pub fn term_is_clean(term: &[u8]) -> bool {
    match std::str::from_utf8(term) {
        Ok(s) => s.nfd().all(|ch| !is_combining_mark(ch)),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(text: &str) -> Vec<String> {
        tokenize(text)
            .into_iter()
            .map(|t| String::from_utf8(t.term).unwrap())
            .collect()
    }

    fn positions(text: &str) -> Vec<(String, u32)> {
        tokenize(text)
            .into_iter()
            .map(|t| (String::from_utf8(t.term).unwrap(), t.position))
            .collect()
    }

    // --- the question that started this module -------------------------------

    #[test]
    fn carriage_returns_newlines_and_tabs_never_reach_a_term() {
        for ws in [
            "\r", "\n", "\t", "\r\n", "   ", "\u{000B}", "\u{000C}", "\u{00A0}", "\u{2028}",
        ] {
            let text = format!("alpha{ws}beta");
            let got = terms(&text);
            assert_eq!(got, vec!["alpha", "beta"], "separator {ws:?}");
            for term in &got {
                assert!(
                    term.chars().all(|c| c.is_alphanumeric()),
                    "term {term:?} still holds whitespace"
                );
            }
        }
    }

    #[test]
    fn a_book_body_of_only_whitespace_yields_nothing() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("\r\n\t \u{00A0}").is_empty());
        assert!(tokenize("   \n\t\r   ").is_empty());
    }

    #[test]
    fn whitespace_inside_words_does_not_merge_them() {
        assert_eq!(terms("thám\n\ttử  sher\tlock"), vec!["tham", "tu", "sher", "lock"]);
    }

    // --- Vietnamese ----------------------------------------------------------

    #[test]
    fn diacritics_are_folded_so_unaccented_queries_match() {
        // The whole reason the indexer normalizes at all.
        for (accented, folded) in [
            ("Thám", "tham"),
            ("tử", "tu"),
            ("Nguyễn", "nguyen"),
            ("Nhật", "nhat"),
            ("Ánh", "anh"),
            ("Yến", "yen"),
            ("Tiệc", "tiec"),
            ("Bão", "bao"),
            ("được", "duoc"),
        ] {
            assert_eq!(terms(accented), vec![folded.to_string()], "{accented}");
        }
        assert_eq!(terms("Thám"), vec!["tham".to_string()]);
        assert_eq!(terms("Nguyễn Nhật Ánh"), vec!["nguyen", "nhat", "anh"]);
        assert_eq!(terms("Nguyen Nhat Anh"), vec!["nguyen", "nhat", "anh"]);
        // Accented and unaccented input converge on the same terms.
        assert_eq!(terms("Thám tử"), terms("tham tu"));
    }

    #[test]
    fn vietnamese_d_stroke_folds_to_plain_d() {
        // `đ` has no combining form, so it needs an explicit fold. Without this,
        // "duoc" never finds "được".
        assert_eq!(
            terms("được đọc đấy ĐƯỢC"),
            vec!["duoc".to_string(), "doc".to_string(), "day".to_string(), "duoc".to_string()]
        );
        assert_eq!(terms("được"), terms("duoc"));
    }

    #[test]
    fn case_is_folded() {
        assert_eq!(terms("SHERLOCK Holmes"), vec!["sherlock", "holmes"]);
        assert_eq!(terms("ẼXAMPLE"), vec!["example".to_string()]);
    }

    #[test]
    fn nfkc_folds_compatibility_forms() {
        // NFKC expands to "1⁄2" and the fraction slash is a separator,
        // which matches what a user typing "1/2" would produce.
        assert_eq!(terms("½"), vec!["1".to_string(), "2".to_string()]);
        assert_eq!(terms("²"), vec!["2".to_string()]);
        assert_eq!(terms("Ａｂ"), vec!["ab".to_string()], "fullwidth latin folds");
    }

    // --- splitting -----------------------------------------------------------

    #[test]
    fn punctuation_splits_terms() {
        assert_eq!(
            terms("don't — well-known (really!) [ok]; yes..."),
            vec![
                "don".to_string(),
                "t".to_string(),
                "well".to_string(),
                "known".to_string(),
                "really".to_string(),
                "ok".to_string(),
                "yes".to_string(),
            ]
        );
    }

    #[test]
    fn underscores_and_hyphens_split() {
        assert_eq!(
            terms("index_split_000"),
            vec!["index".to_string(), "split".to_string(), "000".to_string()]
        );
        assert_eq!(
            terms("Tap 3 - Chuong 17"),
            vec!["tap".to_string(), "3".to_string(), "chuong".to_string(), "17".to_string()]
        );
    }

    #[test]
    fn digits_and_letters_split_into_their_own_terms() {
        assert_eq!(terms("Chapter12"), vec!["chapter".to_string(), "12".to_string()]);
        assert_eq!(terms("3D"), vec!["3".to_string(), "d".to_string()]);
    }

    #[test]
    fn emoji_and_symbols_are_separators() {
        assert_eq!(terms("hello 📚 world"), vec!["hello".to_string(), "world".to_string()]);
        assert_eq!(terms("a+b=c"), vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    // --- CJK -----------------------------------------------------------------

    #[test]
    fn cjk_yields_unigrams_and_bigrams_at_the_documented_positions() {
        // "中文测试" -> unigrams at 0,1,2,3 and bigrams at 0,1,2.
        let got = positions("中文测试");
        let mut unigrams: Vec<(String, u32)> = Vec::new();
        let mut bigrams: Vec<(String, u32)> = Vec::new();
        for (t, p) in &got {
            if t.chars().count() == 1 {
                unigrams.push((t.clone(), *p));
            } else {
                bigrams.push((t.clone(), *p));
            }
        }
        assert_eq!(
            unigrams,
            vec![
                ("中".to_string(), 0),
                ("文".to_string(), 1),
                ("测".to_string(), 2),
                ("试".to_string(), 3),
            ]
        );
        assert_eq!(
            bigrams,
            vec![
                ("中文".to_string(), 0),
                ("文测".to_string(), 1),
                ("测试".to_string(), 2),
            ]
        );
    }

    #[test]
    fn japanese_mixed_script_is_tokenized() {
        // A real title shape: kanji + kana + punctuation.
        let got = terms("日本語のテキスト。");
        assert!(got.contains(&"日".to_string()));
        assert!(got.contains(&"日本".to_string()));
        assert!(got.contains(&"語".to_string()));
        let _ = &got;
        // No single "word" spanning the whole sentence.
        assert!(got.iter().all(|t| t.chars().count() <= 2));
    }

    #[test]
    fn hangul_is_treated_as_spaced_words_not_cjk() {
        // Korean spaces its words, so a Hangul run must not split into unigrams.
        assert_eq!(terms("한국어 텍스트"), vec!["한국어".to_string(), "텍스트".to_string()]);
    }

    #[test]
    fn a_separator_breaks_cjk_adjacency() {
        // No bigram may span the space.
        let got = terms("中 文");
        assert_eq!(got, vec!["中", "文"]);
    }

    #[test]
    fn punctuation_between_cjk_chars_breaks_the_bigram() {
        let got = terms("中、文");
        assert_eq!(got, vec!["中", "文"], "the ideographic comma separates");
    }

    // --- positions -----------------------------------------------------------

    #[test]
    fn positions_are_contiguous_and_monotone() {
        let got = positions("alpha beta gamma");
        let seq: Vec<u32> = got.iter().map(|(_, p)| *p).collect();
        assert_eq!(seq, vec![0, 1, 2]);
    }

    #[test]
    fn positions_continue_across_chunks() {
        // The indexer feeds section after section; the counter must not reset.
        let mut sink = Vec::new();
        let next = for_each_token("alpha beta", 0, |t, p| sink.push((t.to_vec(), p)));
        for_each_token("gamma delta", next, |t, p| sink.push((t.to_vec(), p)));
        let seq: Vec<u32> = sink.iter().map(|(_, p)| *p).collect();
        assert_eq!(seq, vec![0, 1, 2, 3]);
    }

    #[test]
    fn cjk_positions_count_characters_not_bytes() {
        // Multi-byte characters must not advance the counter by more than one.
        let got = positions("日本");
        let seq: Vec<u32> = got.iter().map(|(_, p)| *p).collect();
        assert!(seq.iter().all(|&p| p < 4), "{seq:?}");
    }

    // --- helpers -------------------------------------------------------------

    #[test]
    fn normalize_is_idempotent_on_folded_text() {
        let once = normalize("Thám Tử Sherlock");
        assert_eq!(normalize(&once), once);
    }

    #[test]
    fn every_emitted_term_is_clean() {
        for text in [
            "Thám tử Sherlock ở Kyoto",
            "中文测试",
            "Nguyễn Nhật Ánh",
            "½ portion of ꜱᴍᴀʟʟ ᴄᴀᴘs",
        ] {
            for token in tokenize(text) {
                assert!(
                    term_is_clean(&token.term),
                    "term {:?} still carries a combining mark",
                    String::from_utf8_lossy(&token.term)
                );
            }
        }
    }

    #[test]
    fn invalid_utf8_is_not_a_clean_term() {
        assert!(!term_is_clean(&[0xFF, 0xFE]));
    }

    #[test]
    fn tokenizing_a_whole_book_produces_a_lot_and_nothing_empty() {
        let book = "Sherlock Holmes gặp thám tử Nguyễn Nhật Ánh tại Kyoto. ".repeat(2000);
        let toks = tokenize(&book);
        assert!(toks.len() > 4000);
        assert!(toks.iter().all(|t| !t.term.is_empty()));
        // Positions are 0..n with no gaps.
        let last = toks.len() as u32 - 1;
        assert_eq!(toks.last().unwrap().position, last);
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    /// The scenario the whole question is about: the user searches the folded
    /// form and the engine must still show the words as written in the book.
    #[test]
    fn a_preview_recovers_the_original_words() {
        let text = "Nguyễn Nhật Ánh là tác giả của Yến Tiệc Của Sherlockian.";
        let mut spans: Vec<(String, String)> = Vec::new();
        for_each_token_spanned(text, 0, |term, _, start, end| {
            spans.push((
                String::from_utf8_lossy(term).into_owned(),
                text[start..end].to_string(),
            ));
        });

        let lookup: std::collections::HashMap<&str, &str> = spans
            .iter()
            .map(|(t, orig)| (t.as_str(), orig.as_str()))
            .collect();

        // Every folded term maps back to its accented original.
        assert_eq!(lookup.get("nguyen"), Some(&"Nguyễn"));
        assert_eq!(lookup.get("nhat"), Some(&"Nhật"));
        assert_eq!(lookup.get("anh"), Some(&"Ánh"));
        assert_eq!(lookup.get("yen"), Some(&"Yến"));
        assert_eq!(lookup.get("tiec"), Some(&"Tiệc"));
        assert_eq!(lookup.get("sherlockian"), Some(&"Sherlockian"));
        // Capitalisation is preserved too.
        assert_eq!(lookup.get("sherlockian").map(|s| s.as_bytes()[0]), Some(b'S'));
    }

    #[test]
    fn a_preview_can_show_the_words_around_a_match() {
        // What the UI actually does with a position: slice a window of the
        // original text around it and display that.
        let text = "Sherlock Holmes đến thăm trong Yến Tiệc Của Sherlockian.";
        let mut at = None;
        let mut spans = Vec::new();
        for_each_token_spanned(text, 0, |term, position, start, end| {
            spans.push((term.to_vec(), position, start, end));
            if at.is_none() && term == b"sherlockian" {
                at = Some(position);
            }
        });
        let hit = at.expect("the term is present");
        let (.., s, e) = spans.iter().find(|(_, p, ..)| *p == hit).unwrap();

        // A window of 60 bytes on either side, clamped to the text.
        let win_start = s.saturating_sub(60);
        let win_end = (e + 60).min(text.len());
        let snippet = &text[win_start..win_end];

        assert!(snippet.contains("Yến Tiệc Của Sherlockian"), "{snippet}");
        assert!(snippet.contains("thăm"), "{snippet}");
        // The snippet is the book's own words, not the folded index key: the
        // diacritics and the capital letters are all still there.
        assert!(snippet.contains('ế'), "diacritics must survive");
        assert!(snippet.contains('Ụ') || snippet.contains('Y'), "capitalisation must survive");
        assert!(snippet.contains('S'), "capitalisation must survive");
        assert!(!snippet.contains("yen tiec"), "the folded key must not be shown");
    }

    #[test]
    fn spans_of_cjk_bigrams_cover_both_characters() {
        let text = "中文テスト";
        let mut spans: Vec<(String, String)> = Vec::new();
        for_each_token_spanned(text, 0, |term, _, start, end| {
            spans.push((String::from_utf8_lossy(term).into_owned(), text[start..end].to_string()));
        });
        let lookup: std::collections::HashMap<&str, &str> = spans
            .iter()
            .map(|(t, orig)| (t.as_str(), orig.as_str()))
            .collect();

        // A bigram's span covers both source characters.
        assert_eq!(lookup.get("中文"), Some(&"中文"));
        assert_eq!(lookup.get("文テ"), Some(&"文テ"));
        // And a unigram's span covers just its own.
        assert_eq!(lookup.get("テ"), Some(&"テ"));
    }

    #[test]
    fn spans_are_valid_utf8_boundaries_so_slicing_never_panics() {
        // Multi-byte text sliced at an arbitrary span must not cut a character.
        for text in [
            "Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân",
            "日本語のテキストです。",
            "Thám tử Sherlock gặp được đấy",
            "ẼXAMPLE of ᴇᴠᴇʀʏ capitalization",
        ] {
            for_each_token_spanned(text, 0, |_, _, start, end| {
                assert!(start <= end && end <= text.len(), "range out of bounds");
                assert!(
                    text.is_char_boundary(start),
                    "span start {start} splits a character in {text:?}"
                );
                assert!(
                    text.is_char_boundary(end),
                    "span end {end} splits a character in {text:?}"
                );
            });
        }
    }

    #[test]
    fn a_word_split_across_a_digit_boundary_keeps_its_own_span() {
        let text = "Chapter12 was read.";
        let got: Vec<(String, String)> = {
            let mut v = Vec::new();
            for_each_token_spanned(text, 0, |t, _, s, e| {
                v.push((String::from_utf8_lossy(t).into_owned(), text[s..e].to_string()))
            });
            v
        };
        assert_eq!(
            got,
            vec![
                ("chapter".to_string(), "Chapter".to_string()),
                ("12".to_string(), "12".to_string()),
                ("was".to_string(), "was".to_string()),
                ("read".to_string(), "read".to_string()),
            ]
        );
    }

    #[test]
    fn whitespace_between_words_is_excluded_from_spans() {
        let text = "alpha   \t\r\n  beta";
        let got: Vec<(String, String)> = {
            let mut v = Vec::new();
            for_each_token_spanned(text, 0, |t, _, s, e| {
                v.push((String::from_utf8_lossy(t).into_owned(), text[s..e].to_string()))
            });
            v
        };
        assert_eq!(
            got,
            vec![("alpha".to_string(), "alpha".to_string()), ("beta".to_string(), "beta".to_string())]
        );
    }

    #[test]
    fn spans_agree_with_the_plain_tokenizer() {
        // The spanned and plain walks must produce identical terms in identical
        // order, otherwise the two APIs have drifted apart.
        let texts = [
            "Nguyễn Nhật Ánh",
            "中文测试です",
            "Chapter12 and Tập 3",
            "½ portion",
            "don't — well-known!",
        ];
        for text in texts {
            let plain = tokenize(text);
            let mut spanned = Vec::new();
            for_each_token_spanned(text, 0, |t, p, _, _| spanned.push((t.to_vec(), p)));
            assert_eq!(plain.len(), spanned.len(), "{text:?}");
            for (a, b) in plain.iter().zip(spanned.iter()) {
                assert_eq!(a.term, b.0, "{text:?}");
                assert_eq!(a.position, b.1, "{text:?}");
            }
        }
    }
}

#[cfg(test)]
mod vietnamese_tests {
    use super::*;

    /// Every letter Vietnamese adds beyond basic Latin, plus the whole
    /// precomposed toned-vowel block.
    fn vietnamese_specifics() -> Vec<char> {
        let mut out = vec![
            'ă', 'Ă', 'â', 'Â', 'đ', 'Đ', 'ê', 'Ê', 'ô', 'Ô', 'ơ', 'Ơ', 'ư', 'Ư',
        ];
        for cp in 0x1EA0u32..=0x1EF9 {
            if let Some(ch) = char::from_u32(cp) {
                out.push(ch);
            }
        }
        out
    }

    /// Which characters survive normalization as something other than a single
    /// ASCII letter.
    fn non_collapsing() -> Vec<(char, usize, String)> {
        let mut out = Vec::new();
        for ch in vietnamese_specifics() {
            let n = normalize(&ch.to_string());
            let survives = n.chars().count();
            let exotic = n.chars().any(|c| !c.is_ascii_alphabetic());
            if survives != 1 || exotic || !term_is_clean(n.as_bytes()) {
                out.push((ch, survives, n));
            }
        }
        out
    }

    #[test]
    fn every_vietnamese_letter_collapses_to_one_ascii_letter() {
        // The invariant: nothing Vietnamese may survive as a non-ASCII character
        // or as more than one letter. If `fold_letter` were removed, `đ` and `Đ`
        // would show up here immediately, which is exactly the regression this
        // guards. `o_with_horn_and_u_with_horn_collapse_through_nfd` pins down
        // why `đ` is the only one that needs a hand-written map rather than an
        // NFD pass.
        assert!(
            non_collapsing().is_empty(),
            "these did not collapse: {:?}",
            non_collapsing()
        );
    }

    #[test]
    fn o_with_horn_and_u_with_horn_collapse_through_nfd() {
        // The premise worth refuting: `ơ` and `đ` are NOT the same kind of
        // character. The horn is an assigned combining character, so it is
        // dropped by the ordinary NFD pass; the stroke has no decomposition at
        // all, so it needs `fold_letter`.
        let horn: Vec<char> = 'ơ'.nfd().collect();
        assert_eq!(horn, vec!['o', '\u{031B}'], "the horn is a combining mark");

        let stroke: Vec<char> = 'đ'.nfd().collect();
        assert_eq!(stroke, vec!['đ'], "the stroke has no decomposition");

        assert_eq!(normalize("ơ"), "o");
        assert_eq!(normalize("ư"), "u");
        assert_eq!(normalize("đ"), "d", "only this one needs the explicit fold");
    }

    #[test]
    fn the_fold_covers_both_cases() {
        assert_eq!(normalize("Đ"), "d");
        assert_eq!(normalize("đ"), "d");
        assert_eq!(terms("ĐƯỢC"), vec!["duoc".to_string()]);
    }

    #[test]
    fn every_vietnamese_letter_reaches_a_single_plain_ascii_letter() {
        // Beyond "is it one character", the point is that a Vietnamese word
        // typed without a Vietnamese keyboard still matches.
        for (accented, plain) in [
            ("ăn", "an"),
            ("ân", "an"),
            ("ên", "en"),
            ("ôn", "on"),
            ("ơn", "on"),
            ("ưu", "uu"),
            ("đi", "di"),
            ("Đi", "di"),
            ("cơm", "com"),
            ("tươi", "tuoi"),
            ("mười", "muoi"),
        ] {
            assert_eq!(terms(accented), vec![plain.to_string()], "{accented}");
        }
    }

    #[test]
    fn tone_stripping_merges_distinct_vietnamese_words() {
        // Documented consequence, recorded so it is a known property rather than
        // a surprise: Vietnamese tones are phonemic, and dropping the tone marks
        // makes these six words indistinguishable. This is the price asked for
        // by `note.md` section 0, row 9, so that someone without a Vietnamese
        // keyboard can still find a book. The alternative is indexing both the
        // folded and the exact form, which roughly doubles the term dictionary.
        let forms = ["ta", "tá", "tà", "tả", "tã", "tạ"];
        let folded: Vec<String> = forms.iter().map(|f| terms(f).join(" ")).collect();
        assert!(
            folded.iter().all(|f| *f == "ta"),
            "all tone variants collapse: {folded:?}"
        );
    }

    #[test]
    fn letters_without_a_decomposition_keep_their_identity() {
        // The safety property that makes mark-stripping viable: a character with
        // no canonical decomposition survives untouched, so unrelated scripts do
        // not get folded into each other.
        for (ch, kept) in [
            ('ø', "ø"), // Norwegian: a letter of its own, not "o with something"
            ('æ', "æ"), // Norwegian and Danish
            ('ß', "ß"), // German
            ('ł', "ł"), // Polish
            ('ð', "ð"), // Icelandic
            ('þ', "þ"), // Icelandic
        ] {
            assert_eq!(normalize(&ch.to_string()), kept, "{ch:?}");
        }

        // And the ones that DO decompose, so they fold.
        for (ch, folded) in [
            ('á', "a"), // Spanish and Vietnamese
            ('ñ', "n"), // Spanish
            ('å', "a"), // Swedish and Norwegian: a + combining ring above
            ('ä', "a"), // German and Swedish
            ('ö', "o"), // German and Swedish
        ] {
            assert_eq!(normalize(&ch.to_string()), folded, "{ch:?}");
        }
    }

    #[test]
    fn the_nordic_fold_is_documented_rather_than_surprising() {
        // Consequence worth stating plainly: because `å`, `ä` and `ö` decompose,
        // a Norwegian or Swedish book loses the distinction between "år" (year)
        // and "ar". That is invisible on a Vietnamese library, and the fix is the
        // same one the tone problem needs: index the exact form alongside the
        // folded form. Until then this is a known trade-off, not a subtle bug.
        assert_eq!(terms("år vet vi"), vec!["ar".to_string(), "vet".to_string(), "vi".to_string()]);
    }

    fn terms(text: &str) -> Vec<String> {
        tokenize(text)
            .into_iter()
            .map(|t| String::from_utf8(t.term).unwrap())
            .collect()
    }
}
