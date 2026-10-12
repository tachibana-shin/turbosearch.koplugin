//! Container format detection, metadata extraction, and text decoding.
//!
//! Contents decide the format (`sniff`); the extension only decides whether a
//! file is worth reading at all (`storage::config`). Each text format has its
//! own decoder producing reading-order sections; formats without text still
//! yield metadata where it can be found.

pub mod epub;
pub mod fb2;
pub mod html;
pub mod meta;
pub mod mobi;
pub mod plain;
pub mod sniff;

pub use html::{decode_entities, extract_text};
pub use sniff::detect;
