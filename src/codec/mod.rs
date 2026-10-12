//! Column codecs for the postings store.
//!
//! Each coder is specialised for one *shape* of data, because a single coder for
//! every column is the main reason inverted indexes end up larger than they need
//! to be:
//!
//! | Column    | Shape                                    | Coder          |
//! |-----------|------------------------------------------|----------------|
//! | `doc_id`  | non-decreasing, many duplicates           | Elias-Fano     |
//! | `pos`     | strictly increasing *within one document* | BIC / PForDelta|
//! | `entry`   | constant for long runs                   | RLE            |
//! | terms     | sorted strings, long shared prefixes      | front coding   |
//!
//! See `note.md` section 3.4 for the reasoning and the size budget.

pub mod bic;
pub mod bits;
pub mod elias_fano;
pub mod front;
pub mod pfor;
pub mod rle;
pub mod vbyte;

pub use bic::split_runs;
