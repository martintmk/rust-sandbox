// Licensed under the MIT License.

//! Brotli (RFC 7932): a general-purpose compressor with a static dictionary tuned for web content.
//!
//! Compresses text noticeably better than [`gzip`][crate::gzip] at comparable speed, which is why
//! it is the usual choice for HTTP `Content-Encoding: br`. Requires the `brotli` cargo feature.
//!
//! Brotli streams carry no magic bytes, so the format has to be known from context, such as a
//! `Content-Encoding` header.
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::brotli;
//!
//! let memory = GlobalPool::new();
//! let encoded = brotli::compress(
//!     BytesView::copied_from_slice(b"the quick brown fox", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     brotli::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

mod codec;

use crate::brotli::codec::{BrotliCompress, BrotliDecompress};
use crate::limits::FormatLimits;

/// Brotli's default bounds.
///
/// Brotli has no structural expansion ceiling: measured on ordinary repetitive input it reaches
/// 9 000x for a repeated short string, 10 900x for repetitive JSON, 21 000x for a repeated sentence
/// and 80 660x for 1 MiB of zeros — all legitimate data. A deflate-shaped bound rejects every one of
/// them, so brotli needs its own, and even this one is a coarse backstop rather than real
/// protection.
const DEFAULT_LIMITS: FormatLimits = FormatLimits::new(Some(250_000), None);
use crate::format::macros::define_format;

define_format! {
    name = "brotli",
    encoder_codec = BrotliCompress,
    encoder_options = EncoderOptions,
    new_encoder = |level, options, _pool| BrotliCompress::new(level, options),
    decoder_codec = BrotliDecompress,
    decoder_options = (),
    default_limits = DEFAULT_LIMITS,
    new_decoder = |limits, concatenated, (), _pool| BrotliDecompress::new(limits, concatenated),
    concatenated_default = false,
    concatenated_doc = "Sets whether consecutive brotli streams decode as one logical stream.\n\nDisabled by default: brotli has an explicit end-of-stream marker and concatenation is not an established convention.",
}

/// The kind of data brotli should tune its model for.
///
/// Brotli ships a static dictionary of common web text, and its entropy model can be biased
/// towards a particular kind of input. Choosing correctly is worth a few percent on the ratio;
/// choosing wrongly costs about as much, so leave it at [`Mode::Generic`] unless you know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[non_exhaustive]
pub enum Mode {
    /// No assumption about the input. The default.
    #[default]
    Generic,
    /// UTF-8 text.
    Text,
    /// A WOFF 2.0 font.
    Font,
}

/// The base-2 logarithm of brotli's sliding window, in bytes.
///
/// A larger window finds matches further back, improving the ratio on large inputs at the cost of
/// memory in both the encoder and every decoder that reads the stream. This is a newtype rather
/// than a bare `u8` for the same reason [`Level`] is: an out-of-range value is a configuration
/// mistake to report, not a panic to suffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowSize(u8);

impl WindowSize {
    /// The smallest window brotli accepts, 1 KiB.
    pub const MIN: Self = Self(10);

    /// Brotli's default window, 4 MiB.
    pub const DEFAULT: Self = Self(22);

    /// The largest window brotli accepts without the large-window extension, 16 MiB.
    pub const MAX: Self = Self(24);

    /// Creates a window size from its base-2 exponent, or returns `None` outside `10..=24`.
    #[must_use]
    pub const fn new(exponent: u8) -> Option<Self> {
        if exponent < Self::MIN.0 || exponent > Self::MAX.0 {
            return None;
        }

        Some(Self(exponent))
    }

    /// Returns the base-2 exponent.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for WindowSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for WindowSize {
    type Error = crate::Error;

    fn try_from(exponent: u8) -> core::result::Result<Self, Self::Error> {
        Self::new(exponent).ok_or_else(|| {
            crate::Error::invalid_configuration(format!(
                "brotli window size 2^{exponent} is out of range; expected the exponent in {}..={}",
                Self::MIN.get(),
                Self::MAX.get()
            ))
        })
    }
}

impl From<WindowSize> for u8 {
    fn from(window_size: WindowSize) -> Self {
        window_size.get()
    }
}

/// Brotli's format-specific encoder settings.
///
/// Held by the generated [`EncoderBuilder`] and populated by the setters below.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EncoderOptions {
    pub(crate) mode: Mode,
    pub(crate) window_size: WindowSize,
}

/// Settings that only brotli has.
///
/// The portable settings — [`level`][EncoderBuilder::level] and
/// [`output_chunk_size`][EncoderBuilder::output_chunk_size] — are shared with every other format
/// and are also reachable through [`Format::encoder`][crate::Format::encoder]. These are not: a
/// runtime builder that might produce any format cannot honour a setting only brotli has, so
/// reach for them through this concrete builder and box the result if you need a
/// [`Encoder`][crate::Encoder] trait object.
///
/// ```
/// use bytesbuf::mem::GlobalPool;
/// use compressed::brotli::{Mode, WindowSize};
/// use compressed::{Encoder, brotli};
///
/// let encoder: Box<dyn Encoder> = Box::new(
///     brotli::Encoder::builder()
///         .mode(Mode::Text)
///         .window_size(WindowSize::new(20).expect("20 is in range"))
///         .build(GlobalPool::new()),
/// );
/// # let _ = encoder;
/// ```
impl EncoderBuilder {
    /// Tunes the entropy model for a particular kind of input.
    #[must_use]
    pub const fn mode(mut self, mode: Mode) -> Self {
        self.options.mode = mode;
        self
    }

    /// Sets the sliding window size.
    ///
    /// Every decoder reading the stream must allocate a window this large, so raising it is a cost
    /// paid by the reader as well as the writer.
    #[must_use]
    pub const fn window_size(mut self, window_size: WindowSize) -> Self {
        self.options.window_size = window_size;
        self
    }
}
