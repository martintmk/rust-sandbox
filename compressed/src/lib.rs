// Licensed under the MIT License.

//! # compressed
//!
//! Streaming compression and decompression over [`bytesbuf`] byte sequences.
//!
//! Four formats: [`deflate`], [`zlib`], [`gzip`], and [`brotli`] behind the `brotli` feature.
//! Each lives in its own module and exposes the same six items, so moving between them is a change
//! of import rather than a change of code.
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
//! ## Choosing a format
//!
//! The [`Encoder`] and [`Decoder`] traits describe the contract independently of the format, so
//! code can be written once and used with any of them. When the format is only known at runtime —
//! from a `Content-Encoding` header, say — [`Format`] resolves it and its builders produce a boxed
//! codec, which is itself an `Encoder` or `Decoder` and so fits anywhere a concrete one does:
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::{Format, Level};
//!
//! // Pick the encoding the client ranked highest among those this build supports.
//! let format = Format::from_accept_encoding("br;q=1.0, gzip;q=0.8, deflate;q=0.5")
//!     .next()
//!     .expect("no mutually supported encoding");
//!
//! let memory = GlobalPool::new();
//! let encoded = format.compress(
//!     BytesView::copied_from_slice(b"negotiated", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     format.decompress(encoded, memory)?.to_vec(),
//!     b"negotiated".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```
//!
//! ## Reusing engine state
//!
//! Building a compressor allocates and initialises a substantial amount of state — on a small
//! message, as much work as the compression itself. A service that encodes many messages should
//! hold one [`Pool`], clone it into each encoder, and let the engine return to the pool when the
//! encoder drops. The saving is roughly fixed per message, so it matters most for small bodies.
//!
//! ```
//! use bytesbuf::mem::GlobalPool;
//! use compressed::{Pool, gzip};
//!
//! let codecs = Pool::new();
//! let memory = GlobalPool::new();
//!
//! // Per request: cheap to build, recycles the engine on drop.
//! let encoder = gzip::Encoder::builder().pool(codecs.clone()).build(memory);
//! # let _ = encoder;
//! ```
//!
//! The pool is transparent — it recycles what is worth recycling and builds the rest — so calling
//! code never has to know which engines benefit. See [`Pool`] for what is pooled today.
//!
//! ## Security
//!
//! Every one of these formats can expand its input by orders of magnitude, so a decoder pointed at
//! untrusted data is a memory-exhaustion vector.
//!
//! The codecs themselves never accumulate: each `pull` hands back one bounded chunk, so nothing in
//! this crate grows with the length of the stream. The exposure belongs to whatever the caller does
//! with those chunks, which is why the limits matter most for the accumulating conveniences —
//! `compress`, `decompress`, and [`Format::compress`] / [`Format::decompress`].
//!
//! Each format declares its own default bounds, because a single portable ratio cannot serve both
//! families. Deflate cannot expand by more than about 1032x — a structural property of the format —
//! so the deflate family defaults to 1100x and never rejects data it could legitimately have
//! produced. Brotli has no such ceiling: measured on ordinary repetitive input it reaches 9 000x
//! for a repeated short string, 21 000x for a repeated sentence and 80 660x for a megabyte of
//! zeros, so it defaults to 250 000x.
//!
//! [`DecompressionLimits`] carries *overrides*, not values: bounds you leave unset keep the
//! format's default, so [`DecompressionLimits::default()`] never silently imposes one format's
//! calibration on another.
//!
//! **A ratio limit is therefore a coarse backstop, not real protection.** For untrusted input, set
//! [`DecompressionLimits::with_max_output_len`] to whatever the caller can actually afford to
//! buffer. Use [`DecompressionLimits::UNLIMITED`] only for sources you trust as much as your own
//! process.
//!
//! ## Features
//!
//! * `brotli` — the [`brotli`] module and [`Format::Brotli`], via the pure-Rust `brotli` crate.
//! * `futures-stream` — `CompressStream` and `DecompressStream`, which present compression and
//!   decompression as a [`futures_core::Stream`] over any stream of byte sequences.
//!
//! Both are off by default, so the base build pulls in nothing beyond `bytesbuf` and `flate2`.

#[cfg(feature = "brotli")]
pub mod brotli;
mod codec;
#[cfg(feature = "deflate")]
pub mod deflate;
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
mod engine;
mod error;
#[cfg(any(feature = "deflate", feature = "gzip", feature = "zlib"))]
mod flate;
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
pub mod format;
#[cfg(feature = "gzip")]
pub mod gzip;
mod level;
mod limits;
mod output;
mod pool;
#[cfg(feature = "zlib")]
pub mod zlib;
#[cfg(feature = "zstd")]
pub mod zstd;

#[cfg(feature = "futures-stream")]
mod stream;

pub use codec::{Decoder, Encoder};
pub use error::{Error, Result};
#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
pub use format::Format;
pub use level::Level;
pub use limits::DecompressionLimits;
pub use output::Output;
pub use pool::Pool;
#[cfg(feature = "futures-stream")]
pub use stream::{CompressStream, DecompressStream};
