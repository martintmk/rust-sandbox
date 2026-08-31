// Licensed under the MIT License.

//! Zstandard (RFC 8878): fast compression with ratios well beyond the deflate family.
//!
//! The usual choice when both speed and ratio matter, and the format behind HTTP
//! `Content-Encoding: zstd`. Requires the `zstd` cargo feature.
//!
//! Unlike this crate's other formats, zstd is provided by a C library compiled from bundled
//! sources, so enabling it requires a C compiler. Builds that leave the feature off stay pure Rust.
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::zstd;
//!
//! let memory = GlobalPool::new();
//! let encoded = zstd::compress(
//!     BytesView::copied_from_slice(b"the quick brown fox", &memory),
//!     memory.clone(),
//! )?;
//! assert_eq!(encoded.range(0..4).to_vec(), vec![0x28, 0xb5, 0x2f, 0xfd]);
//!
//! assert_eq!(
//!     zstd::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

mod codec;

use crate::format::macros::define_format;
use crate::limits::FormatLimits;
use crate::zstd::codec::{ZstdCompress, ZstdDecompress};

/// Zstd's default bounds.
///
/// Zstd has no structural expansion ceiling, so like brotli it needs a far looser ratio than the
/// deflate family. This is a coarse backstop rather than real protection; see
/// [`DecompressionLimits`] for what actually bounds an untrusted stream.
const DEFAULT_LIMITS: FormatLimits = FormatLimits::new(Some(250_000), None);

define_format! {
    name = "zstd",
    encoder_codec = ZstdCompress,
    encoder_options = EncoderOptions,
    new_encoder = ZstdCompress::new,
    decoder_codec = ZstdDecompress,
    decoder_options = (),
    default_limits = DEFAULT_LIMITS,
    new_decoder = |limits, concatenated, (), pool| ZstdDecompress::new(limits, concatenated, pool),
    concatenated_default = true,
    concatenated_doc = "Sets whether concatenated zstd frames decode as one logical stream.\n\nEnabled by default, matching the `zstd` command line tool.",
}

/// A level on zstd's own scale, for reaching settings the portable [`Level`] does not cover.
///
/// The portable scale is anchored on zstd's default so that [`Level::DEFAULT`] means the same
/// thing on every format, which leaves zstd's slowest levels unreachable. They are rarely worth it
/// — measured on realistic JSON, level 19 is over 200 times slower than level 3 for about 17%
/// better compression — but this is how to ask for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompressionLevel(i32);

impl CompressionLevel {
    /// The fastest level zstd offers.
    pub const MIN: Self = Self(1);

    /// Zstd's own default, which the portable [`Level::DEFAULT`] also maps to.
    pub const DEFAULT: Self = Self(3);

    /// The strongest level zstd offers.
    pub const MAX: Self = Self(22);

    /// Creates a level, or returns `None` outside zstd's range of `1..=22`.
    ///
    /// Negative levels, which zstd uses for its "fast" modes, are deliberately not accepted: they
    /// trade ratio for speed in a way already covered by the low end of the portable scale.
    #[must_use]
    pub const fn new(level: i32) -> Option<Self> {
        if level < Self::MIN.0 || level > Self::MAX.0 {
            return None;
        }

        Some(Self(level))
    }

    /// Returns the level on zstd's scale.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<i32> for CompressionLevel {
    type Error = crate::Error;

    fn try_from(level: i32) -> core::result::Result<Self, Self::Error> {
        Self::new(level).ok_or_else(|| {
            crate::Error::invalid_configuration(format!(
                "zstd compression level {level} is out of range; expected {}..={}",
                Self::MIN.0,
                Self::MAX.0
            ))
        })
    }
}

impl From<CompressionLevel> for i32 {
    fn from(level: CompressionLevel) -> Self {
        level.get()
    }
}

/// Zstd's format-specific encoder settings.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct EncoderOptions {
    pub(crate) level: Option<CompressionLevel>,
}

/// Settings that only zstd has.
///
/// ```
/// use bytesbuf::mem::GlobalPool;
/// use compressed::zstd::{self, CompressionLevel};
///
/// let encoder = zstd::Encoder::builder()
///     .compression_level(CompressionLevel::new(19).expect("19 is in range"))
///     .build(GlobalPool::new());
/// # let _ = encoder;
/// ```
impl EncoderBuilder {
    /// Sets the level on zstd's own scale, overriding any portable [`Level`].
    ///
    /// Use this only when you need a level the portable scale does not reach; prefer
    /// [`level`][EncoderBuilder::level] otherwise, so the same configuration keeps working if the
    /// format changes.
    #[must_use]
    pub const fn compression_level(mut self, level: CompressionLevel) -> Self {
        self.options.level = Some(level);
        self
    }
}
