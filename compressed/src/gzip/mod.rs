// Licensed under the MIT License.

//! Gzip compression and decompression.
//!
//! [`Encoder`] and [`Decoder`] are incremental push/pull state machines, so a stream of any length
//! can be processed with a bounded working set. For data that is already in memory, [`compress`]
//! and [`decompress`] run the loop for you.
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::gzip;
//!
//! let memory = GlobalPool::new();
//! let plain = BytesView::copied_from_slice(b"the quick brown fox", &memory);
//!
//! let encoded = gzip::compress(plain, memory.clone())?;
//! assert_eq!(encoded.range(0..2).to_vec(), vec![0x1f, 0x8b]);
//!
//! assert_eq!(
//!     gzip::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

mod decoder;
mod encoder;

use bytesbuf::BytesView;
use bytesbuf::mem::MemoryShared;

use crate::error::Result;
pub use crate::gzip::decoder::{Decoder, DecoderBuilder};
pub use crate::gzip::encoder::{Encoder, EncoderBuilder};

/// Compresses a complete byte sequence that is already in memory.
///
/// Uses [`Level::DEFAULT`][crate::Level::DEFAULT]. Prefer [`Encoder`] for data that arrives
/// incrementally; this convenience buffers the entire result before returning.
///
/// # Errors
///
/// Returns an error if the underlying compression engine fails.
pub fn compress(input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
    let mut encoder = Encoder::new(memory);
    encoder.push(input)?;
    encoder.finish();

    // After `finish` the encoder never asks for more input, so `into_data` returning `None` means
    // the stream ended.
    let mut parts = Vec::new();
    while let Some(chunk) = encoder.pull()?.into_data() {
        parts.push(chunk);
    }

    Ok(BytesView::from_views(parts))
}

/// Decompresses a complete gzip stream that is already in memory.
///
/// Applies [`DecompressionLimits::DEFAULT`][crate::DecompressionLimits::DEFAULT]. Prefer
/// [`Decoder`] for data that arrives incrementally; this convenience buffers the entire result
/// before returning.
///
/// # Errors
///
/// Returns an error if the data is not valid gzip, is truncated, or exceeds the default limits.
pub fn decompress(input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
    let mut decoder = Decoder::new(memory);
    decoder.push(input)?;
    decoder.finish();

    // After `finish` the decoder never asks for more input: it either produces data, ends, or
    // reports truncation as an error.
    let mut parts = Vec::new();
    while let Some(chunk) = decoder.pull()?.into_data() {
        parts.push(chunk);
    }

    Ok(BytesView::from_views(parts))
}
