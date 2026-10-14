//! The `CONFIG` table and the mutable search/indexing settings.
//!
//! Every user-tunable knob lives here (`note.md` sections 3.5 and 4.1.1). Rust
//! never reads a config file: the Lua side pushes changes through
//! `config_set()` over the FFI boundary.

use super::pack::{Reader, Writer};
use super::{Engine, RunResult};

/// Extensions that are scanned by default, `note.md` section 3.5.1.
pub const DEFAULT_INCLUDE: &[&str] = &[
    "epub", "kepub", "mobi", "azw", "azw3", "txt", "text", "md", "markdown", "fb2", "fb2.zip",
    "html", "htm", "xhtml", "xht", "shtml",
];

/// Extensions that are never scanned, `note.md` section 3.5.1.
///
/// Covers junk that shows up next to books (KOReader sidecars, cover art,
/// partial downloads) plus formats we deliberately do not index.
pub const DEFAULT_EXCLUDE: &[&str] = &[
    // Junk that turns up next to real books.
    "log", "lua", "json", "opf", "ncx", "css", "js", "db", "db3", "sqlite3", "bak", "tmp", "temp",
    "part", "partial", "old", "orig", "swp", "sdr", "toc", "cover",
    // Images, fonts, archives.
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "mp3", "mp4", "mkv", "woff", "woff2",
    "ttf", "zip", "gz", "tgz", "bz2", "xz", "7z", "rar", "tar", "zst",
    // Deliberately unindexed formats.
    "pdf", "djvu", "djv", "cbz", "cbr", "cb7", "cbt", "azw4", "azw6", "azw8", "kfx", "tpz",
];

/// Directory globs skipped wholesale, `note.md` section 3.5.2.
pub const DEFAULT_EXCLUDE_DIRS: &[&str] =
    &["*.sdr", ".calibrece", ".thumbnails", "@eaDir", "__MACOSX"];

/// Default minimum file size: below this a "book" is a fragment or a stray note.
pub const DEFAULT_MIN_FILE_SIZE: u64 = 5 * 1024;
/// Default maximum file size: guards against a pathological single file.
pub const DEFAULT_MAX_FILE_SIZE: u64 = 64 * 1024 * 1024;

/// How a multi-term query handles terms that are not complete words.
///
/// `note.md` section 4.1.1: typing `cha` on a Kindle keyboard usually means
/// `chai`, so a term absent from the dictionary is treated as a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum PrefixMode {
    /// Expand only terms that are absent from the dictionary.
    #[default]
    Auto = 0,
    /// Never expand; match complete terms only.
    Exact = 1,
    /// Always expand every term as a prefix.
    Prefix = 2,
}

impl PrefixMode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(PrefixMode::Auto),
            1 => Some(PrefixMode::Exact),
            2 => Some(PrefixMode::Prefix),
            _ => None,
        }
    }

    /// The name the text form uses, and the label a settings screen shows.
    pub fn name(&self) -> &'static str {
        match self {
            PrefixMode::Auto => "auto",
            PrefixMode::Exact => "exact",
            PrefixMode::Prefix => "prefix",
        }
    }

    /// Reads one of those names back.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(PrefixMode::Auto),
            "exact" => Some(PrefixMode::Exact),
            "prefix" => Some(PrefixMode::Prefix),
            _ => None,
        }
    }
}

/// How many positions to keep per (term, document).
///
/// `usize::MAX` means "keep every position". `note.md` section 1.1.1 measured the
/// cost of each setting on a 614 EPUB library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexDepth(pub u32);

impl IndexDepth {
    /// Keep every position: 102.9 MB on the measured 1.36 GB library.
    pub const FULL: IndexDepth = IndexDepth(u32::MAX);
    /// 39.5 MB on the measured library.
    pub const BALANCED: IndexDepth = IndexDepth(32);
    /// 20.1 MB on the measured library.
    pub const LIGHT: IndexDepth = IndexDepth(8);

    pub fn is_full(&self) -> bool {
        self.0 == u32::MAX
    }

    /// The name the text form uses, or `None` for a stop somebody chose.
    ///
    /// The three measured settings are named because the third of them is
    /// `u32::MAX`, and printing that would show a reader `4294967295` as though
    /// it were a limit anyone picked. It is not a setting, it is how "keep them
    /// all" is spelled — so it says `full`, and a number between the measured
    /// three stays a number, which is what makes it editable.
    pub fn name(&self) -> Option<&'static str> {
        if self.0 == IndexDepth::FULL.0 {
            Some("full")
        } else if self.0 == IndexDepth::BALANCED.0 {
            Some("balanced")
        } else if self.0 == IndexDepth::LIGHT.0 {
            Some("light")
        } else {
            None
        }
    }
}

/// How much of a book's content reaches the index.
///
/// This is a different axis from [`IndexDepth`], which caps *how many positions*
/// are kept per term: `ContentMode` decides whether content is indexed at all.
/// Splitting them keeps "index everything" and "index less of everything" from
/// being the same dial.
///
/// Measured on the 614-book reference library, all three over the same text:
///
/// | Mode | Postings | Database |
/// |---|---|---|
/// | `Full` | 49,9 M | 163 MB |
/// | `TermsOnly` | ~8 M (one per term and book) | ~40 MB |
/// | `FileList` | 0 | ~1 MB |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ContentMode {
    /// Content and positions, so phrase queries and snippets work.
    #[default]
    Full = 0,
    /// Content, but one posting per term and book — enough to answer "which
    /// books contain this word", not "where in them".
    TermsOnly = 1,
    /// Metadata only, exactly as for a format whose text we cannot decode. The
    /// cheapest useful index: a filename and title search over the whole library.
    FileList = 2,
}

impl ContentMode {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ContentMode::Full),
            1 => Some(ContentMode::TermsOnly),
            2 => Some(ContentMode::FileList),
            _ => None,
        }
    }

    /// True when content postings are written at all.
    pub fn has_content(&self) -> bool {
        !matches!(self, ContentMode::FileList)
    }

    /// True when positions are stored, so phrase queries and snippets can work.
    pub fn has_positions(&self) -> bool {
        matches!(self, ContentMode::Full)
    }

    /// Parses the name `--mode` and `CONTENT_MODE` both accept.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "full" => Some(ContentMode::Full),
            "terms" => Some(ContentMode::TermsOnly),
            "files" => Some(ContentMode::FileList),
            _ => None,
        }
    }

    /// The name the UI and the logs use, which is also what `CONTENT_MODE`
    /// accepts as an override.
    pub fn name(&self) -> &'static str {
        match self {
            ContentMode::Full => "full",
            ContentMode::TermsOnly => "terms",
            ContentMode::FileList => "files",
        }
    }
}

impl std::fmt::Display for ContentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl Default for IndexDepth {
    fn default() -> Self {
        IndexDepth::FULL
    }
}

/// Every user-tunable setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Roots to scan. Empty means "nowhere configured yet".
    pub source_dirs: Vec<String>,
    pub include_extensions: Vec<String>,
    pub exclude_extensions: Vec<String>,
    pub exclude_dir_globs: Vec<String>,
    pub min_file_size: u64,
    pub max_file_size: u64,
    /// Positions kept per (term, document).
    pub index_depth: IndexDepth,
    /// Prefix expansion behaviour for multi-term queries.
    pub prefix_mode: PrefixMode,
    /// How much content to index. See [`ContentMode`].
    pub content_mode: ContentMode,
    /// Only index while the device is on external power.
    pub index_only_when_charging: bool,
    /// Index metadata (but not content) for formats we cannot parse.
    pub meta_only_unparseable: bool,
}

/// Reads a boolean the way a hand-edited settings file writes one.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

/// Why a settings line could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A line without a `=`.
    #[error("{0:?} is not a key = value line")]
    Line(String),
    /// A value the setting does not accept.
    #[error("{0:?} does not accept that value")]
    Value(String),
    /// A setting that cannot be changed by rewriting the config.
    #[error("{0:?} changes what is stored, so it can only be chosen when the index is built")]
    ReadOnly(String),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_dirs: vec!["/mnt/us/documents".to_string()],
            include_extensions: DEFAULT_INCLUDE.iter().map(|s| s.to_string()).collect(),
            exclude_extensions: DEFAULT_EXCLUDE.iter().map(|s| s.to_string()).collect(),
            exclude_dir_globs: DEFAULT_EXCLUDE_DIRS.iter().map(|s| s.to_string()).collect(),
            min_file_size: DEFAULT_MIN_FILE_SIZE,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            index_depth: IndexDepth::default(),
            prefix_mode: PrefixMode::default(),
            content_mode: ContentMode::Full,
            index_only_when_charging: true,
            meta_only_unparseable: false,
        }
    }
}

impl Config {
    /// The `CONFIG` key. One row holds the whole settings blob.
    pub const KEY: &'static str = "config";

    pub fn encode(&self) -> Vec<u8> {
        Writer::with_capacity(256)
            .str_list(&self.source_dirs)
            .str_list(&self.include_extensions)
            .str_list(&self.exclude_extensions)
            .str_list(&self.exclude_dir_globs)
            .u64(self.min_file_size)
            .u64(self.max_file_size)
            .u32(self.index_depth.0)
            .u8(self.prefix_mode as u8)
            .bool(self.index_only_when_charging)
            .bool(self.meta_only_unparseable)
            .u8(self.content_mode as u8)
            .finish()
    }

    pub fn decode(bytes: &[u8]) -> super::pack::Result<Self> {
        let mut r = Reader::new(bytes);
        let cfg = Config {
            source_dirs: r.str_list()?,
            include_extensions: r.str_list()?,
            exclude_extensions: r.str_list()?,
            exclude_dir_globs: r.str_list()?,
            min_file_size: r.u64()?,
            max_file_size: r.u64()?,
            index_depth: IndexDepth(r.u32()?),
            prefix_mode: PrefixMode::from_u8(r.u8()?).unwrap_or_default(),
            index_only_when_charging: r.bool()?,
            meta_only_unparseable: r.bool()?,
            content_mode: ContentMode::from_u8(r.u8().unwrap_or(0)).unwrap_or_default(),
        };
        Ok(cfg)
    }

    /// Hash of the settings that change the token stream.
    ///
    /// Only these force a rebuild when changed (`note.md` section 3.5.3). The
    /// file-selection settings are deliberately excluded: changing them only
    /// needs a rescan, because postings of existing books are untouched.
    pub fn content_hash(&self) -> u32 {
        let mut h: u32 = 0x811C_9DC5;
        let feed = |bytes: &[u8], h: &mut u32| {
            for &b in bytes {
                *h ^= b as u32;
                *h = h.wrapping_mul(0x0100_0193);
            }
        };
        feed(&self.index_depth.0.to_le_bytes(), &mut h);
        h
    }

    /// Roots to scan, as `PathBuf`s.
    pub fn roots(&self) -> Vec<std::path::PathBuf> {
        self.source_dirs
            .iter()
            .map(std::path::PathBuf::from)
            .collect()
    }

    /// The filter derived from these settings.
    pub fn filter(&self) -> super::book::FileFilter {
        super::book::FileFilter {
            include: self.include_extensions.clone(),
            exclude: self.exclude_extensions.clone(),
            exclude_dir_globs: self.exclude_dir_globs.clone(),
            min_size: self.min_file_size,
            max_size: self.max_file_size,
        }
    }

    /// True when `ext` (lowercase, no dot) would be handed to the sniffer.
    pub fn accepts_extension(&self, ext: &str) -> bool {
        let ext = ext.to_ascii_lowercase();
        if self.exclude_extensions.contains(&ext) {
            return false;
        }
        self.include_extensions.contains(&ext)
    }

    /// The settings as text, one `key = value` pair per line.
    ///
    /// [`Self::encode`] is for storage; this is for a reader. A setting is only
    /// worth having if somebody can see it and change it, and a settings screen
    /// cannot offer a field it cannot render. The format is the obvious one, so a
    /// hand-edited file is a valid answer too.
    ///
    /// Lists are comma-separated: every list here holds names, and a name that
    /// wants a comma can be quoted on the way in.
    pub fn to_lines(&self) -> Vec<(String, String)> {
        vec![
            ("source_dirs", self.source_dirs.join(",")),
            ("include_extensions", self.include_extensions.join(",")),
            ("exclude_extensions", self.exclude_extensions.join(",")),
            ("exclude_dir_globs", self.exclude_dir_globs.join(",")),
            ("min_file_size", self.min_file_size.to_string()),
            ("max_file_size", self.max_file_size.to_string()),
            (
                "index_depth",
                self.index_depth
                    .name()
                    .map(str::to_string)
                    .unwrap_or_else(|| self.index_depth.0.to_string()),
            ),
            ("prefix_mode", self.prefix_mode.name().into()),
            (
                "index_only_when_charging",
                self.index_only_when_charging.to_string(),
            ),
            (
                "meta_only_unparseable",
                self.meta_only_unparseable.to_string(),
            ),
            ("content_mode", self.content_mode.name().into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Applies the `key = value` lines in `text`, leaving anything absent alone.
    ///
    /// Partial on purpose. A settings screen sends back the fields it shows, and
    /// treating a missing line as a request to reset that field would let an older
    /// reader silently undo a newer setting it has no field for. An unknown key is
    /// left alone for the same reason the other way round.
    ///
    /// `content_mode` is refused rather than applied: it decides what is stored, so
    /// changing it would leave postings in the shape the old mode produced — the
    /// results would come back from an index that no longer means what it says. The
    /// refusal names the fix, which is to rebuild.
    pub fn merge_lines(&mut self, text: &str) -> Result<(), ConfigError> {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::Line(line.to_string()));
            };
            let (key, value) = (key.trim(), value.trim());
            let list = || -> Vec<String> {
                value
                    .split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part: &String| !part.is_empty())
                    .collect()
            };
            match key {
                "source_dirs" => self.source_dirs = list(),
                "include_extensions" => self.include_extensions = list(),
                "exclude_extensions" => self.exclude_extensions = list(),
                "exclude_dir_globs" => self.exclude_dir_globs = list(),
                "min_file_size" => {
                    self.min_file_size =
                        value.parse().map_err(|_| ConfigError::Value(key.into()))?;
                }
                "max_file_size" => {
                    self.max_file_size =
                        value.parse().map_err(|_| ConfigError::Value(key.into()))?;
                }
                "index_depth" => {
                    // The three named settings are what a reader chooses between; a
                    // bare number is accepted too, because `note.md` measured the
                    // cost of each and somebody will want a stop between two.
                    let depth = match value {
                        "full" => IndexDepth::FULL,
                        "balanced" => IndexDepth::BALANCED,
                        "light" => IndexDepth::LIGHT,
                        other => other
                            .parse()
                            .map(IndexDepth)
                            .map_err(|_| ConfigError::Value(key.into()))?,
                    };
                    self.index_depth = depth;
                }
                "prefix_mode" => {
                    self.prefix_mode = PrefixMode::from_name(value)
                        .ok_or_else(|| ConfigError::Value(key.into()))?;
                }
                "index_only_when_charging" => {
                    self.index_only_when_charging =
                        parse_bool(value).ok_or_else(|| ConfigError::Value(key.into()))?;
                }
                "meta_only_unparseable" => {
                    self.meta_only_unparseable =
                        parse_bool(value).ok_or_else(|| ConfigError::Value(key.into()))?;
                }
                "content_mode" => return Err(ConfigError::ReadOnly(key.into())),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Engine {
    /// Reads the stored config, or defaults when nothing is stored.
    pub fn config(&self) -> RunResult<Config> {
        // No table-exists branch: the schema is created on open, so an empty
        // table means the user never set anything.
        match self.config_raw()? {
            Some(b) => {
                Config::decode(&b).map_err(|e| crate::storage::RunError::Message(e.to_string()))
            }
            None => Ok(Config::default()),
        }
    }

    /// Writes the whole config in one transaction.
    pub fn set_config(&self, cfg: &Config) -> RunResult<()> {
        self.write_config_raw(&cfg.encode())
    }
}
