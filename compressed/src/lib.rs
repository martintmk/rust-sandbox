// Licensed under the MIT License.

//! # compressed
//!
//! Streaming compression and decompression over [`bytesbuf`] byte sequences. Gzip is the only
//! format supported today, in the [`gzip`] module.
//!
//! Compression engines normally speak `std::io::Read` and `std::io::Write`, which assume a single
//! contiguous `&[u8]`. A [`BytesView`][bytesbuf::BytesView] is a chain of segments with no
//! contiguous representation, so bridging the two through `std::io` would mean copying every byte
//! into a flat buffer first. This crate drives the engine from the view's segments directly, and
//! writes into the uninitialized spare capacity of a [`BytesBuf`][bytesbuf::BytesBuf], so no
//! intermediate copy is needed.
//!
//! ## Whole buffers
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::gzip;
//!
//! let memory = GlobalPool::new();
//! let encoded = gzip::compress(
//!     BytesView::copied_from_slice(b"hello", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     gzip::decompress(encoded, memory)?.to_vec(),
//!     b"hello".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```
//!
//! ## Streaming
//!
//! [`gzip::Encoder`] and [`gzip::Decoder`] are push/pull state machines rather than one-shot
//! transforms. Each `pull` returns at most one chunk, so processing a multi-gigabyte stream never
//! holds more than one pending input view plus one output chunk:
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::{Output, gzip};
//!
//! # let memory = GlobalPool::new();
//! # let source = vec![gzip::compress(
//! #     BytesView::copied_from_slice(b"streamed", &memory), memory.clone())?];
//! let mut decoder = gzip::Decoder::new(memory);
//! let mut chunks = source.into_iter();
//! let mut plain = Vec::new();
//!
//! loop {
//!     match decoder.pull()? {
//!         Output::Data(data) => plain.push(data),
//!         Output::NeedInput => match chunks.next() {
//!             Some(chunk) => decoder.push(chunk)?,
//!             None => decoder.finish(),
//!         },
//!         Output::Done => break,
//!     }
//! }
//!
//! assert_eq!(BytesView::from_views(plain).to_vec(), b"streamed".to_vec());
//! # Ok::<(), compressed::Error>(())
//! ```
//!
//! The [`Encoder`] and [`Decoder`] traits describe that contract independently of the format, so
//! callers can be generic over it.
//!
//! ## Security
//!
//! Gzip can expand its input by orders of magnitude, so a decoder pointed at untrusted data is a
//! memory-exhaustion vector. [`gzip::Decoder`] therefore applies
//! [`DecompressionLimits::DEFAULT`], which rejects expansion beyond 1000x while placing no cap on
//! total size, so legitimate large streams still decode. Tighten it with
//! [`gzip::DecoderBuilder::limits`], or opt out entirely with [`DecompressionLimits::UNLIMITED`]
//! when the source is trusted.
//!
//! ## Features
//!
//! * `futures-stream` — `CompressStream` and `DecompressStream`, which present compression and
//!   decompression as a [`futures_core::Stream`] over any stream of byte sequences.

mod codec;
mod engine;
mod error;
pub mod gzip;
mod level;
mod limits;
mod output;

#[cfg(feature = "futures-stream")]
mod stream;

pub use codec::{Decoder, Encoder};
pub use error::{Error, Result};
pub use level::Level;
pub use limits::DecompressionLimits;
pub use output::Output;
#[cfg(feature = "futures-stream")]
pub use stream::{CompressStream, DecompressStream};
