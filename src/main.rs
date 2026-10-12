//! CLI entry point. Thin wrapper over the library so all real logic is
//! reachable from the future `cdylib` FFI target as well.

use turbosearch::codec;

fn main() {
    println!("turbosearch {}", env!("CARGO_PKG_VERSION"));
    println!("codec layer ready: bif, elias_fano, front, pfor, rle, vbyte");
    println!("split_runs re-exported: {}", codec::split_runs(&[1, 1, 2]).len());
}
