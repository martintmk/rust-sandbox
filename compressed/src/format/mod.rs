// Licensed under the MIT License.

//! Choosing a compression format at runtime.
//!
//! The format modules (`gzip` and friends) are the right choice when the format is
//! known at compile time. This module is for when it is not: encoding whatever a client asked for,
//! or decoding whatever a peer declared it sent.
//!
//! [`Format`] is the entry point. The builders it returns live here beside it, so they do not
//! collide with the per-format builders such as
//! [`gzip::CompressorBuilder`][crate::gzip::CompressorBuilder].

#[cfg(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))]
pub(crate) mod macros;

use std::cmp::Reverse;
use std::num::NonZeroUsize;

use bytesbuf::BytesView;
use bytesbuf::mem::MemoryShared;

use crate::compression::{Compress, Compression, Decompress};
use crate::engine::DEFAULT_CHUNK_SIZE;
use crate::error::Result;
use crate::level::Level;
use crate::limits::DecompressionLimits;
use crate::pool::Pool;

/// A compression format, selectable at runtime.
///
/// The format modules (`gzip` and friends) are the right choice when the format is
/// known at compile time. This enum is for when it is not: encoding whatever a client asked for,
/// or decoding whatever a peer declared it sent.
///
/// # Examples
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::GlobalPool;
/// use compressed::Level;
/// use compressed::format::Format;
///
/// // The format arrives as a string, from an HTTP header.
/// let format = Format::from_content_encoding("gzip").expect("a supported encoding");
///
/// let memory = GlobalPool::new();
/// let mut compressor = format.compressor().level(Level::BEST).build(memory.clone());
///
/// compressor.push(BytesView::copied_from_slice(b"payload", &memory))?;
/// # Ok::<(), compressed::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// Raw deflate, RFC 1951. See `deflate`. Requires the `deflate` feature.
    #[cfg(feature = "deflate")]
    Deflate,
    /// Zlib, RFC 1950. See `zlib`. Requires the `zlib` feature.
    #[cfg(feature = "zlib")]
    Zlib,
    /// Gzip, RFC 1952. See `gzip`. Requires the `gzip` feature.
    #[cfg(feature = "gzip")]
    Gzip,
    /// Brotli, RFC 7932. See `brotli`. Requires the `brotli` feature.
    #[cfg(feature = "brotli")]
    Brotli,
    /// Zstandard, RFC 8878. See `zstd`. Requires the `zstd` feature.
    #[cfg(feature = "zstd")]
    Zstd,
}

impl Format {
    /// Every format this build supports, in no particular order.
    ///
    /// The contents depend on which cargo features are enabled.
    pub const ALL: &'static [Self] = &[
        #[cfg(feature = "deflate")]
        Self::Deflate,
        #[cfg(feature = "zlib")]
        Self::Zlib,
        #[cfg(feature = "gzip")]
        Self::Gzip,
        #[cfg(feature = "brotli")]
        Self::Brotli,
        #[cfg(feature = "zstd")]
        Self::Zstd,
    ];

    /// The HTTP `Content-Encoding` token for this format, if it has one.
    ///
    /// Returns `None` for `Format::Deflate`: raw deflate has no HTTP token. Note that HTTP's
    /// `deflate` token means a *zlib* stream, not raw deflate, so it maps to `Format::Zlib`.
    #[must_use]
    #[cfg_attr(
        not(feature = "deflate"),
        expect(
            clippy::unnecessary_wraps,
            reason = "raw deflate is the only format without an HTTP token, and it is not enabled in this configuration"
        )
    )]
    pub const fn content_encoding(self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "deflate")]
            Self::Deflate => None,
            #[cfg(feature = "zlib")]
            Self::Zlib => Some("deflate"),
            #[cfg(feature = "gzip")]
            Self::Gzip => Some("gzip"),
            #[cfg(feature = "brotli")]
            Self::Brotli => Some("br"),
            #[cfg(feature = "zstd")]
            Self::Zstd => Some("zstd"),
        }
    }

    /// Parses a single HTTP `Content-Encoding` token.
    ///
    /// Matching is case-insensitive, as HTTP requires. `deflate` maps to `Format::Zlib`, which is
    /// what the token actually denotes; `x-gzip` is accepted as a legacy alias for `gzip`. Tokens
    /// for formats this build does not support return `None`.
    ///
    /// This takes one bare token. To choose an encoding from a client's `Accept-Encoding` header,
    /// which carries a weighted list, use [`Format::from_accept_encoding`].
    #[must_use]
    pub fn from_content_encoding(token: &str) -> Option<Self> {
        let token = token.trim();

        #[cfg(feature = "gzip")]
        if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            return Some(Self::Gzip);
        }

        #[cfg(feature = "zlib")]
        if token.eq_ignore_ascii_case("deflate") {
            return Some(Self::Zlib);
        }

        #[cfg(feature = "brotli")]
        if token.eq_ignore_ascii_case("br") {
            return Some(Self::Brotli);
        }

        #[cfg(feature = "zstd")]
        if token.eq_ignore_ascii_case("zstd") {
            return Some(Self::Zstd);
        }

        #[cfg(not(any(feature = "brotli", feature = "gzip", feature = "zlib", feature = "zstd")))]
        let _ = token;

        None
    }

    /// Lists the encodings a client accepts, most preferred first.
    ///
    /// Real `Accept-Encoding` headers are weighted lists such as `br;q=1.0, gzip;q=0.8, *;q=0.1`,
    /// so a bare token match is not enough. This parses the quality values, discards encodings this
    /// build does not support, and yields the rest in preference order, keeping the client's order
    /// on a tie.
    ///
    /// It yields *every* acceptable encoding rather than picking one, because the choice is often
    /// the caller's: a server may decline an encoding the client would accept, for its cost or for
    /// compatibility. Filter the iterator and take the first survivor.
    ///
    /// A quality of zero means the client explicitly refuses that encoding, so it is never yielded.
    /// `identity` and `*` are ignored — an empty iterator simply means sending the body
    /// uncompressed, which is always acceptable.
    ///
    /// # Examples
    ///
    /// ```
    /// use compressed::format::Format;
    ///
    /// // Take the client's first choice.
    /// let best = Format::from_accept_encoding("gzip;q=0.8, deflate;q=0.5").next();
    /// assert_eq!(best, Some(Format::Gzip));
    ///
    /// // Or apply your own policy on top of the client's ranking.
    /// let affordable = Format::from_accept_encoding("br;q=1.0, gzip;q=0.8")
    ///     .find(|format| *format != Format::Brotli);
    /// assert_eq!(affordable, Some(Format::Gzip));
    ///
    /// // A quality of zero is a refusal, not a low ranking.
    /// assert_eq!(
    ///     Format::from_accept_encoding("gzip;q=0, deflate").next(),
    ///     Some(Format::Zlib)
    /// );
    ///
    /// assert_eq!(Format::from_accept_encoding("identity").count(), 0);
    /// ```
    pub fn from_accept_encoding(header: &str) -> impl Iterator<Item = Self> + use<> {
        // At most one entry per supported format, so the ranking never allocates however long or
        // repetitive the header is.
        let mut ranked: [Option<Ranked>; MAX_ACCEPTED] = [None; MAX_ACCEPTED];
        let mut len = 0;

        for entry in header.split(',') {
            let mut parts = entry.split(';');

            let Some(format) = parts.next().and_then(Self::from_content_encoding) else {
                continue;
            };

            // A malformed weight makes the entry unusable; skipping it is safer than guessing a
            // preference the client did not express.
            let Some(quality) = parse_quality(parts) else {
                continue;
            };

            // Zero means "not acceptable", which is a refusal rather than a low ranking.
            if quality == 0 {
                continue;
            }

            // A repeated encoding keeps its strongest weight and its first position.
            if let Some(existing) = ranked[..len]
                .iter_mut()
                .filter_map(Option::as_mut)
                .find(|ranked| ranked.format == format)
            {
                existing.quality = existing.quality.max(quality);
                continue;
            }

            ranked[len] = Some(Ranked {
                format,
                quality,
                order: len,
            });
            len += 1;
        }

        // Sorting on the original position as well keeps ties in the order the client wrote them.
        ranked[..len].sort_unstable_by_key(|entry| entry.map_or((Reverse(0), usize::MAX), |entry| (Reverse(entry.quality), entry.order)));

        // `use<>` keeps the header's lifetime out of the return type: every entry is copied into
        // the array above, so the caller is free to drop the header immediately.
        ranked.into_iter().take(len).flatten().map(|entry| entry.format)
    }

    /// Starts configuring a compressor for this format.
    #[must_use]
    pub const fn compressor(self) -> CompressorBuilder {
        CompressorBuilder {
            format: self,
            level: Level::DEFAULT,
            chunk_size: default_chunk_size(),
            pool: None,
        }
    }

    /// Starts configuring a decompressor for this format.
    #[must_use]
    pub const fn decompressor(self) -> DecompressorBuilder {
        DecompressorBuilder {
            format: self,
            limits: DecompressionLimits::new(),
            chunk_size: default_chunk_size(),
            multi_stream: None,
            pool: None,
        }
    }

    /// Compresses a complete byte sequence that is already in memory.
    ///
    /// Uses [`Level::DEFAULT`]; for anything else, configure a compressor with [`Format::compressor`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying compression engine fails.
    pub fn compress(self, input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
        self.compressor().build(memory).compress(input)
    }

    /// Decompresses a complete stream that is already in memory.
    ///
    /// Applies [`DecompressionLimits::new()`]; for anything else, configure a decompressor with
    /// [`Format::decompressor`].
    ///
    /// # Errors
    ///
    /// Returns an error if the data is malformed, truncated, or exceeds the default limits.
    pub fn decompress(self, input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
        self.decompressor().build(memory).decompress(input)
    }
}

/// The most encodings a ranking can hold: one per format this build supports.
const MAX_ACCEPTED: usize = Format::ALL.len();

/// One acceptable encoding, with the weight and position it was given.
#[derive(Debug, Clone, Copy)]
struct Ranked {
    format: Format,
    quality: u16,
    order: usize,
}

/// Reads the `q=` weight from an entry's parameters, as thousandths.
///
/// Returns `Some(1000)` when no weight is given, since an absent quality means full preference.
/// Returns `None` if a weight is present but malformed.
fn parse_quality<'a>(parameters: impl Iterator<Item = &'a str>) -> Option<u16> {
    let mut quality = 1_000;

    for parameter in parameters {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };

        if !name.trim().eq_ignore_ascii_case("q") {
            continue;
        }

        quality = parse_quality_value(value.trim())?;
    }

    Some(quality)
}

/// Parses a quality value in `0..=1` with up to three decimals, as thousandths.
///
/// Parsed as integers rather than a float so the ordering is exact and the accepted grammar matches
/// what the HTTP specification actually allows.
fn parse_quality_value(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));

    let whole: u16 = whole.parse().ok()?;
    if whole > 1 || fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    // Pad so "5" and "500" both mean 0.5.
    let mut thousandths = 0_u16;
    for index in 0..3 {
        thousandths = thousandths * 10 + u16::from(fraction.as_bytes().get(index).map_or(b'0', |byte| *byte) - b'0');
    }

    let quality = whole * 1_000 + thousandths;
    if quality > 1_000 { None } else { Some(quality) }
}

const fn default_chunk_size() -> NonZeroUsize {
    match NonZeroUsize::new(DEFAULT_CHUNK_SIZE) {
        Some(size) => size,
        None => NonZeroUsize::MIN,
    }
}

/// Configures a compressor for a [`Format`] chosen at runtime.
///
/// Mirrors the per-format builders such as [`gzip::CompressorBuilder`][crate::gzip::CompressorBuilder],
/// but produces a boxed compressing [`Compression`] so the format need not be known at compile time. Reach it
/// through [`Format::compressor`] rather than naming it directly.
#[derive(Debug, Clone)]
pub struct CompressorBuilder {
    format: Format,
    level: Level,
    chunk_size: NonZeroUsize,
    pool: Option<Pool>,
}

impl CompressorBuilder {
    /// Sets the compression level, mapped onto the format's native range.
    #[must_use]
    pub const fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Sets how much output a single `pull` produces before returning.
    #[must_use]
    pub const fn output_chunk_size(mut self, bytes: NonZeroUsize) -> Self {
        self.chunk_size = bytes;
        self
    }

    /// Recycles engine state through a shared [`Pool`].
    ///
    /// Building a compressor is not free, so a service that compresses many messages should hand every
    /// compressor the same pool. The engine is returned when the compressor is dropped. Without a pool
    /// each compressor builds its own engine, which is the default.
    #[must_use]
    pub fn pool(mut self, pool: Pool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Builds the compressor, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Box<dyn Compression<Mode = Compress>> {
        macro_rules! build {
            ($module:ident) => {{
                let builder = crate::$module::Compressor::builder()
                    .level(self.level)
                    .output_chunk_size(self.chunk_size);

                let builder = match self.pool {
                    Some(pool) => builder.pool(pool),
                    None => builder,
                };

                Box::new(builder.build(memory))
            }};
        }

        match self.format {
            #[cfg(feature = "deflate")]
            Format::Deflate => build!(deflate),
            #[cfg(feature = "zlib")]
            Format::Zlib => build!(zlib),
            #[cfg(feature = "gzip")]
            Format::Gzip => build!(gzip),
            #[cfg(feature = "brotli")]
            Format::Brotli => build!(brotli),
            #[cfg(feature = "zstd")]
            Format::Zstd => build!(zstd),
        }
    }
}

/// Configures a decompressor for a [`Format`] chosen at runtime.
///
/// Mirrors the per-format builders such as [`gzip::DecompressorBuilder`][crate::gzip::DecompressorBuilder],
/// but produces a boxed decompressing [`Compression`] so the format need not be known at compile time. Reach it
/// through [`Format::decompressor`] rather than naming it directly.
#[derive(Debug, Clone)]
pub struct DecompressorBuilder {
    format: Format,
    limits: DecompressionLimits,
    chunk_size: NonZeroUsize,
    multi_stream: Option<bool>,
    pool: Option<Pool>,
}

impl DecompressorBuilder {
    /// Overrides the bounds on how much data decompression may produce.
    ///
    /// Bounds left unset on the passed value keep the chosen format's own defaults, which differ by
    /// orders of magnitude between the deflate family and brotli.
    ///
    /// # Security
    ///
    /// Set [`with_max_output_len`][DecompressionLimits::with_max_output_len] when the data comes
    /// from an untrusted peer.
    #[must_use]
    pub const fn limits(mut self, limits: DecompressionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets how much output a single `pull` produces before returning.
    #[must_use]
    pub const fn output_chunk_size(mut self, bytes: NonZeroUsize) -> Self {
        self.chunk_size = bytes;
        self
    }

    /// Sets whether consecutive streams decompress as one logical stream.
    ///
    /// Left unset, each format keeps its own default: enabled for `Format::Gzip` and
    /// `Format::Zstd`, matching `gzip(1)` and the `zstd` tool, and disabled for the rest, where
    /// concatenation is not an established convention.
    ///
    /// This also decides what happens to bytes trailing a complete stream. When enabled they must
    /// themselves form another valid stream, so padding or appended data is reported as corrupt.
    /// When disabled they are silently ignored, which means the same appended bytes are rejected
    /// under the formats that default to enabled and accepted under those that do not.
    #[must_use]
    pub const fn multi_stream(mut self, enabled: bool) -> Self {
        self.multi_stream = Some(enabled);
        self
    }

    /// Recycles engine state through a shared [`Pool`].
    ///
    /// The engine is returned when the decompressor is dropped. See [`Pool`] for which engines are
    /// actually recycled.
    #[must_use]
    pub fn pool(mut self, pool: Pool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Builds the decompressor, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Box<dyn Compression<Mode = Decompress>> {
        macro_rules! build {
            ($module:ident) => {{
                let builder = crate::$module::Decompressor::builder()
                    .limits(self.limits)
                    .output_chunk_size(self.chunk_size);

                let builder = match self.multi_stream {
                    Some(enabled) => builder.multi_stream(enabled),
                    None => builder,
                };

                let builder = match self.pool {
                    Some(pool) => builder.pool(pool),
                    None => builder,
                };

                Box::new(builder.build(memory))
            }};
        }

        match self.format {
            #[cfg(feature = "deflate")]
            Format::Deflate => build!(deflate),
            #[cfg(feature = "zlib")]
            Format::Zlib => build!(zlib),
            #[cfg(feature = "gzip")]
            Format::Gzip => build!(gzip),
            #[cfg(feature = "brotli")]
            Format::Brotli => build!(brotli),
            #[cfg(feature = "zstd")]
            Format::Zstd => build!(zstd),
        }
    }
}

#[cfg(test)]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;
    use crate::Output;

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    #[cfg(feature = "gzip")]
    fn accepted(header: &str) -> Vec<Format> {
        Format::from_accept_encoding(header).collect()
    }

    fn compressed_len(builder: CompressorBuilder, payload: &[u8]) -> usize {
        let mut compressor = builder.build(GlobalPool::new());
        compressor.push(view(payload)).expect("push succeeds");
        compressor.finish();

        let mut total = 0;
        while let Some(chunk) = compressor.pull().expect("pull succeeds").into_data() {
            total += chunk.len();
        }

        total
    }

    #[test]
    fn every_format_round_trips_through_the_enum() {
        let payload = b"runtime selected format ".repeat(200);

        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let compressed = format.compress(view(&payload), memory.clone()).expect("compression succeeds");
            let plain = format.decompress(compressed, memory).expect("decompression succeeds");

            assert_eq!(plain.to_vec(), payload, "{format:?} failed to round trip");
        }
    }

    #[test]
    fn content_encoding_tokens_round_trip() {
        for &format in Format::ALL {
            let Some(token) = format.content_encoding() else {
                continue;
            };

            assert_eq!(
                Format::from_content_encoding(token),
                Some(format),
                "{format:?} did not survive its own token"
            );
        }
    }

    #[cfg(all(feature = "deflate", feature = "zlib"))]
    #[test]
    fn http_deflate_token_means_zlib() {
        // The most common source of confusion in this area: HTTP's `deflate` token denotes a zlib
        // stream, not raw deflate.
        assert_eq!(Format::from_content_encoding("deflate"), Some(Format::Zlib));
        assert_eq!(Format::Deflate.content_encoding(), None);
    }

    #[cfg(all(feature = "gzip", feature = "zlib"))]
    #[test]
    fn accept_encoding_honours_quality_values() {
        // Real headers are weighted; a bare token match would pick whichever came first.
        assert_eq!(accepted("gzip;q=0.5, deflate;q=0.9"), vec![Format::Zlib, Format::Gzip]);
        assert_eq!(accepted("gzip;q=0.9, deflate;q=0.5"), vec![Format::Gzip, Format::Zlib]);

        // An absent weight means full preference.
        assert_eq!(accepted("deflate;q=0.5, gzip"), vec![Format::Gzip, Format::Zlib]);
    }

    #[cfg(all(feature = "gzip", feature = "zlib"))]
    #[test]
    fn accept_encoding_treats_zero_quality_as_a_refusal() {
        // The trap a naive "strip the parameters" implementation falls into: q=0 means the client
        // will not accept that encoding at all.
        assert_eq!(accepted("gzip;q=0"), vec![]);
        assert_eq!(accepted("gzip;q=0.000"), vec![]);
        assert_eq!(
            accepted("gzip;q=0, deflate"),
            vec![Format::Zlib],
            "the refused encoding is dropped entirely"
        );
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn accept_encoding_tolerates_real_world_headers() {
        for header in [
            "gzip",
            " gzip ",
            "gzip;q=1",
            "gzip;q=1.0",
            "gzip ; q=1.0",
            "GZIP;Q=1.0",
            "identity;q=0.1, gzip;q=0.9",
            "*;q=0.1, gzip",
            "br;q=0.2, gzip;q=0.9",
        ] {
            assert_eq!(
                Format::from_accept_encoding(header).next(),
                Some(Format::Gzip),
                "failed on {header:?}"
            );
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn accept_encoding_ignores_what_it_cannot_use() {
        assert_eq!(accepted(""), vec![]);
        assert_eq!(accepted("identity"), vec![]);
        assert_eq!(accepted("*"), vec![], "a wildcard names no specific encoding");
        assert_eq!(accepted("lzma, snappy"), vec![], "unsupported encodings");

        // A malformed weight is skipped rather than guessed at.
        assert_eq!(accepted("gzip;q=nonsense"), vec![]);
        assert_eq!(accepted("gzip;q=2"), vec![], "quality above 1 is invalid");
        assert_eq!(accepted("gzip;q=0.1234"), vec![], "more than three decimals");
    }

    #[cfg(all(feature = "gzip", feature = "zlib"))]
    #[test]
    fn accept_encoding_keeps_client_order_on_a_tie() {
        assert_eq!(accepted("gzip;q=0.5, deflate;q=0.5"), vec![Format::Gzip, Format::Zlib]);
        assert_eq!(accepted("deflate;q=0.5, gzip;q=0.5"), vec![Format::Zlib, Format::Gzip]);
    }

    #[cfg(all(feature = "gzip", feature = "zlib"))]
    #[test]
    fn a_repeated_encoding_keeps_its_strongest_weight() {
        // A ranking holds one entry per format, so a repetitive header cannot overflow it.
        assert_eq!(accepted("gzip;q=0.1, deflate;q=0.5, gzip;q=0.9"), vec![Format::Gzip, Format::Zlib]);
        assert_eq!(accepted(&"gzip;q=0.5, ".repeat(100)), vec![Format::Gzip]);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn the_ranking_does_not_borrow_the_header() {
        // The header is often a temporary; the iterator must outlive it.
        let ranking = {
            let header = String::from("gzip;q=0.9");
            Format::from_accept_encoding(&header)
        };

        assert_eq!(ranking.collect::<Vec<_>>(), vec![Format::Gzip]);
    }

    #[cfg(all(feature = "gzip", feature = "zlib"))]
    #[test]
    fn a_caller_can_apply_its_own_policy_on_top_of_the_ranking() {
        // The reason this yields every acceptable encoding instead of picking one.
        let chosen = Format::from_accept_encoding("gzip;q=1.0, deflate;q=0.9").find(|format| *format != Format::Gzip);

        assert_eq!(chosen, Some(Format::Zlib));
    }

    #[test]
    fn quality_values_parse_as_exact_thousandths() {
        assert_eq!(parse_quality_value("1"), Some(1_000));
        assert_eq!(parse_quality_value("1.0"), Some(1_000));
        assert_eq!(parse_quality_value("1.000"), Some(1_000));
        assert_eq!(parse_quality_value("0"), Some(0));
        assert_eq!(parse_quality_value("0.5"), Some(500));
        assert_eq!(parse_quality_value("0.05"), Some(50));
        assert_eq!(parse_quality_value("0.005"), Some(5));

        assert_eq!(parse_quality_value("1.001"), None, "above 1 is invalid");
        assert_eq!(parse_quality_value("2"), None);
        assert_eq!(parse_quality_value("-1"), None);
        assert_eq!(parse_quality_value("0.abc"), None);
        assert_eq!(parse_quality_value(""), None);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn content_encoding_parsing_is_case_insensitive_and_trims() {
        assert_eq!(Format::from_content_encoding("GZIP"), Some(Format::Gzip));
        assert_eq!(Format::from_content_encoding("  gzip  "), Some(Format::Gzip));
        assert_eq!(Format::from_content_encoding("x-gzip"), Some(Format::Gzip));
        assert_eq!(Format::from_content_encoding("identity"), None);
        assert_eq!(Format::from_content_encoding(""), None);
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn brotli_uses_the_br_token() {
        assert_eq!(Format::from_content_encoding("br"), Some(Format::Brotli));
        assert_eq!(Format::Brotli.content_encoding(), Some("br"));
    }

    #[cfg(not(feature = "brotli"))]
    #[test]
    fn brotli_token_is_rejected_when_the_feature_is_off() {
        assert_eq!(Format::from_content_encoding("br"), None);
    }

    #[cfg(not(feature = "gzip"))]
    #[test]
    fn gzip_token_is_rejected_when_the_feature_is_off() {
        assert_eq!(Format::from_content_encoding("gzip"), None);
    }

    #[test]
    fn the_compressor_builder_applies_its_level() {
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(400);

        for &format in Format::ALL {
            let fast = compressed_len(format.compressor().level(Level::FAST), &payload);
            let best = compressed_len(format.compressor().level(Level::BEST), &payload);

            assert!(best <= fast, "{format:?}: best={best} should not exceed fast={fast}");
        }
    }

    #[test]
    fn the_compressor_builder_applies_its_chunk_size() {
        let bound = NonZeroUsize::new(128).expect("128 is not zero");

        for &format in Format::ALL {
            let mut compressor = format.compressor().output_chunk_size(bound).build(GlobalPool::new());
            compressor.push(view(&b"chunked ".repeat(5_000))).expect("push succeeds");
            compressor.finish();

            while let Some(chunk) = compressor.pull().expect("pull succeeds").into_data() {
                assert!(chunk.len() <= bound.get(), "{format:?} produced a {} byte chunk", chunk.len());
            }
        }
    }

    #[test]
    fn the_decompressor_builder_applies_its_limits() {
        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let compressed = format
                .compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone())
                .expect("compression succeeds");

            let mut decompressor = format
                .decompressor()
                .limits(DecompressionLimits::new().without_max_ratio().with_max_output_len(1024))
                .build(memory);
            decompressor.push(compressed).expect("push succeeds");
            decompressor.finish();

            let error = loop {
                match decompressor.pull() {
                    Ok(Output::Data(_)) => {}
                    Ok(_) => panic!("{format:?}: the cap should have fired"),
                    Err(error) => break error,
                }
            };

            assert!(error.is_limit_exceeded(), "{format:?}: got {error}");
        }
    }

    #[test]
    fn multi_stream_governs_every_format() {
        // The generic half of the contract: whatever the format, setting this explicitly decides
        // whether a second stream is decompressed or ignored.
        let memory = GlobalPool::new();
        let payload = b"member ".repeat(50);

        for &format in Format::ALL {
            let compressed = format.compress(view(&payload), memory.clone()).expect("compress");
            let joined = BytesView::from_views([compressed.clone(), compressed]);

            let joined_len = decompressed_len(format.decompressor().multi_stream(true).build(memory.clone()), joined.clone());
            assert_eq!(joined_len, payload.len() * 2, "{format:?} should join with multi_stream(true)");

            let single_len = decompressed_len(format.decompressor().multi_stream(false).build(memory.clone()), joined);
            assert_eq!(single_len, payload.len(), "{format:?} should stop with multi_stream(false)");
        }
    }

    #[test]
    fn each_format_keeps_its_own_multi_stream_default() {
        // The format-specific half: the runtime builder must preserve each format's own default
        // rather than flattening every format to one behaviour. Gzip and zstd join, matching
        // `gzip(1)` and the `zstd` tool; the rest stop at the first stream.
        let memory = GlobalPool::new();
        let payload = b"member ".repeat(50);

        for &format in Format::ALL {
            // Matching the variant by name keeps this free of the cfg gates the variants carry.
            let joins_by_default = matches!(format!("{format:?}").as_str(), "Gzip" | "Zstd");

            let compressed = format.compress(view(&payload), memory.clone()).expect("compress");
            let joined = BytesView::from_views([compressed.clone(), compressed]);

            let len = decompressed_len(format.decompressor().build(memory.clone()), joined);
            let expected = if joins_by_default { payload.len() * 2 } else { payload.len() };

            assert_eq!(len, expected, "{format:?} did not keep its documented default");
        }
    }

    fn decompressed_len(decompressor: Box<dyn Compression<Mode = Decompress>>, input: BytesView) -> usize {
        decompressor.decompress(input).expect("decompression succeeds").len()
    }

    #[test]
    fn all_lists_exactly_the_compiled_in_formats() {
        let expected = usize::from(cfg!(feature = "deflate"))
            + usize::from(cfg!(feature = "zlib"))
            + usize::from(cfg!(feature = "gzip"))
            + usize::from(cfg!(feature = "brotli"))
            + usize::from(cfg!(feature = "zstd"));

        assert_eq!(Format::ALL.len(), expected);
    }
}
