//! Index writing and reading.

pub mod dict;
pub mod entrymap;
pub mod posting;
pub mod query;
pub mod writer;

pub use dict::{Dict, DictEntry};
pub use entrymap::{Builder as EntryMapBuilder, CHECKPOINT_INTERVAL, EntryMap};
pub use posting::{into_chunks, Chunk, Posting, CHUNK_POSTINGS, CODEC_TAG};
