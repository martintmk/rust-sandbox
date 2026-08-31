// Licensed under the MIT License.

//! Compressing and decompressing a whole buffer.
//!
//! Run with `cargo run --example round_trip --all-features`.

use bytesbuf::BytesView;
use bytesbuf::mem::GlobalPool;
use compressed::{Format, Result, gzip};

fn main() -> Result<()> {
    // Every output buffer is allocated from this provider.
    let memory = GlobalPool::new();
    let original = b"the quick brown fox jumps over the lazy dog. ".repeat(64);

    let encoded = gzip::compress(BytesView::copied_from_slice(&original, &memory), memory.clone())?;
    let decoded = gzip::decompress(encoded.clone(), memory.clone())?;

    assert_eq!(decoded.to_vec(), original);
    println!("gzip: {} -> {} bytes", original.len(), encoded.len());

    // The same payload through a format chosen at run time.
    for &format in Format::ALL {
        let input = BytesView::copied_from_slice(&original, &memory);
        let encoded = format.compress(input, memory.clone())?;
        let decoded = format.decompress(encoded.clone(), memory.clone())?;

        assert_eq!(decoded.to_vec(), original);
        println!("{format:?}: {} bytes", encoded.len());
    }

    Ok(())
}
