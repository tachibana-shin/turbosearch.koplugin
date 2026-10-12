//! Dumps the first bytes of a MOBI file's records for manual diagnosis.
//!
//! ```text
//! cargo run --release --bin mobihex -- <file.mobi> [record_index] [count]
//! ```
use turbosearch::format::mobi::{parse, Record};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("path");
    let which: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    let count: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(64);
    let bytes = std::fs::read(&path)?;
    let m = parse(&bytes)?;

    // Recover the record table by re-parsing the header the same way `parse` does.
    let record_count = u16::from_be_bytes([bytes[76], bytes[77]]) as usize;
    let mut records: Vec<Record> = Vec::with_capacity(record_count);
    for i in 0..record_count {
        let base = 78 + i * 8;
        let offset = u32::from_be_bytes([bytes[base], bytes[base+1], bytes[base+2], bytes[base+3]]) as usize;
        let next = if i + 1 < record_count {
            let b = 78 + (i + 1) * 8;
            u32::from_be_bytes([bytes[b], bytes[b+1], bytes[b+2], bytes[b+3]]) as usize
        } else { bytes.len() };
        records.push(Record { offset, len: next - offset });
    }
    eprintln!("records: {}, record0 len {}, record1 len {}", record_count, records[0].len,
        records.get(1).map(|r| r.len).unwrap_or(0));

    let r = records[which];
    let slice = &bytes[r.offset..r.offset + r.len];
    eprintln!("--- record {which} first {count} bytes ---");
    for (i, chunk) in slice[..count.min(slice.len())].chunks(16).enumerate() {
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let ascii: String = chunk.iter().map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' }).collect();
        eprintln!("{:04x}  {hex:<48} {ascii}", i * 16);
    }
    eprintln!("--- decoded output, first {count} bytes ---");
    for (i, chunk) in m.text[..count.min(m.text.len())].chunks(16).enumerate() {
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let ascii: String = chunk.iter().map(|&b| if (0x20..=0x7e).contains(&b) { b as char } else { '.' }).collect();
        eprintln!("{:04x}  {hex:<48} {ascii}", i * 16);
    }
    Ok(())
}
