//! Where the reference library is, for tests and binaries that need real books.
//!
//! Several checks only make sense against a real library: the PalmDOC decoder is
//! validated by decoding actual MOBI files, the span invariant is checked against
//! actual Vietnamese EPUB text, and the postings codec is measured on a corpus
//! nobody hand-picked. None of that can be faked convincingly.
//!
//! But the library lives on one developer's machine, so nothing here may assume
//! it. A hardcoded absolute path breaks the moment anyone else clones the
//! repository, and worse, it does so *silently* on a test that returns early:
//! CI reports ten tests passed when ten tests ran zero assertions. That is the
//! failure this module exists to prevent.

use std::path::{Path, PathBuf};

/// Environment variable that overrides the library location.
///
/// Set it in CI, or on a second machine, rather than editing code.
pub const ENV_VAR: &str = "TURBOSEARCH_LIBRARY";

/// The reference library root, or `None` when it is not available.
pub fn root() -> Option<PathBuf> {
    let explicit = std::env::var(ENV_VAR).ok().map(PathBuf::from);
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    resolve(explicit, home)
}

/// True when the library is available, for tests that report why they skipped.
pub fn is_available() -> bool {
    root().is_some()
}

/// Resolves a library root from an explicit override and a home directory.
///
/// Kept free of `std::env` on purpose: mutating the environment from a test is
/// both unsafe in edition 2024 and racy against the other tests running in the
/// same process, so the decision is a pure function the tests can drive.
///
/// Resolution order:
/// 1. the explicit override, if it points at a directory
/// 2. the KOReader default location inside the home directory
pub fn resolve(explicit: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.is_dir().then_some(p);
    }
    let candidate =
        Path::new(home?.as_os_str()).join(".config/koreader/rakuyomi/downloads/calibre");
    candidate.is_dir().then_some(candidate)
}

/// A scratch directory for a throwaway database.
///
/// Never a hardcoded `/tmp/opencode`: that path exists on the machine that
/// developed this and nowhere else, which is exactly the kind of assumption that
/// turns a green build red for everyone else. `std::env::temp_dir` points at the
/// platform's own temporary directory, which always exists.
pub fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join("turbosearch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_override_wins_and_must_be_a_directory() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().to_path_buf();
        assert_eq!(
            resolve(Some(p.clone()), Some(PathBuf::from("/home/x"))),
            Some(p)
        );
        // A path that exists but is not a directory must not resolve.
        let file = d.path().join("a-file");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            resolve(Some(file), Some(PathBuf::from("/home/x"))),
            None,
            "a file is not a library"
        );
    }

    #[test]
    fn a_missing_override_falls_back_to_the_koreader_default() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().to_path_buf();
        let expected = home.join(".config/koreader/rakuyomi/downloads/calibre");
        std::fs::create_dir_all(&expected).unwrap();
        assert_eq!(resolve(None, Some(home.clone())), Some(expected));
        // ...and a home without a library resolves to nothing, not to a path
        // that would fail later.
        let bare = tempfile::tempdir().unwrap();
        assert_eq!(resolve(None, Some(bare.path().to_path_buf())), None);
    }

    #[test]
    fn a_home_directory_is_required_when_no_override_is_given() {
        assert_eq!(resolve(None, None), None);
    }

    #[test]
    fn the_scratch_directory_sits_under_the_platform_temp_dir() {
        let d = scratch_dir();
        assert_eq!(d.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(d.file_name().unwrap(), "turbosearch");
    }
}
