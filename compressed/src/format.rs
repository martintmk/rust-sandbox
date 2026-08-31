// Licensed under the MIT License.

use std::num::NonZeroUsize;

use bytesbuf::BytesView;
use bytesbuf::mem::MemoryShared;

use crate::codec::{Decoder, Encoder};
use crate::engine::DEFAULT_CHUNK_SIZE;
use crate::error::Result;
use crate::level::Level;
use crate::limits::DecompressionLimits;

/// A compression format, selectable at runtime.
///
/// The format modules ([`gzip`][crate::gzip] and friends) are the right choice when the format is
/// known at compile time. This enum is for when it is not: encoding whatever a client asked for,
/// or decoding whatever a peer declared it sent.
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::GlobalPool;
/// use compressed::{Format, Level};
///
/// // The format arrives as a string, from an HTTP header.
/// let format = Format::from_content_encoding("gzip").expect("a supported encoding");
///
/// let memory = GlobalPool::new();
/// let mut encoder = format.encoder().level(Level::BEST).build(memory.clone());
///
/// encoder.push(BytesView::copied_from_slice(b"payload", &memory))?;
/// # Ok::<(), compressed::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// Raw deflate, RFC 1951. See [`deflate`][crate::deflate].
    Deflate,
    /// Zlib, RFC 1950. See [`zlib`][crate::zlib].
    Zlib,
    /// Gzip, RFC 1952. See [`gzip`][crate::gzip].
    Gzip,
    /// Brotli, RFC 7932. See [`brotli`][crate::brotli]. Requires the `brotli` feature.
    #[cfg(feature = "brotli")]
    Brotli,
}

impl Format {
    /// Every format this build supports, in no particular order.
    ///
    /// The contents depend on which cargo features are enabled.
    pub const ALL: &'static [Self] = &[
        Self::Deflate,
        Self::Zlib,
        Self::Gzip,
        #[cfg(feature = "brotli")]
        Self::Brotli,
    ];

    /// The HTTP `Content-Encoding` token for this format, if it has one.
    ///
    /// Returns `None` for [`Format::Deflate`]: raw deflate has no HTTP token. Note that HTTP's
    /// `deflate` token means a *zlib* stream, not raw deflate, so it maps to [`Format::Zlib`].
    #[must_use]
    pub const fn content_encoding(self) -> Option<&'static str> {
        match self {
            Self::Deflate => None,
            Self::Zlib => Some("deflate"),
            Self::Gzip => Some("gzip"),
            #[cfg(feature = "brotli")]
            Self::Brotli => Some("br"),
        }
    }

    /// Parses an HTTP `Content-Encoding` or `Accept-Encoding` token.
    ///
    /// Matching is case-insensitive, as HTTP requires. `deflate` maps to [`Format::Zlib`], which is
    /// what the token actually denotes; `x-gzip` is accepted as a legacy alias for `gzip`. Tokens
    /// for formats this build does not support return `None`, so a caller scanning a preference
    /// list falls through to the next encoding the client offered.
    #[must_use]
    pub fn from_content_encoding(token: &str) -> Option<Self> {
        let token = token.trim();

        if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            return Some(Self::Gzip);
        }

        if token.eq_ignore_ascii_case("deflate") {
            return Some(Self::Zlib);
        }

        #[cfg(feature = "brotli")]
        if token.eq_ignore_ascii_case("br") {
            return Some(Self::Brotli);
        }

        None
    }

    /// Starts configuring an encoder for this format.
    #[must_use]
    pub const fn encoder(self) -> EncoderBuilder {
        EncoderBuilder {
            format: self,
            level: Level::DEFAULT,
            chunk_size: default_chunk_size(),
        }
    }

    /// Starts configuring a decoder for this format.
    #[must_use]
    pub const fn decoder(self) -> DecoderBuilder {
        DecoderBuilder {
            format: self,
            limits: DecompressionLimits::DEFAULT,
            chunk_size: default_chunk_size(),
            concatenated: None,
        }
    }

    /// Compresses a complete byte sequence that is already in memory.
    ///
    /// Uses [`Level::DEFAULT`]; for anything else, configure an encoder with [`Format::encoder`].
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying compression engine fails.
    pub fn compress(self, input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
        let mut encoder = self.encoder().build(memory);
        encoder.push(input)?;
        encoder.finish();

        drain(|| encoder.pull())
    }

    /// Decompresses a complete stream that is already in memory.
    ///
    /// Applies [`DecompressionLimits::DEFAULT`]; for anything else, configure a decoder with
    /// [`Format::decoder`].
    ///
    /// # Errors
    ///
    /// Returns an error if the data is malformed, truncated, or exceeds the default limits.
    pub fn decompress(self, input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
        let mut decoder = self.decoder().build(memory);
        decoder.push(input)?;
        decoder.finish();

        drain(|| decoder.pull())
    }
}

const fn default_chunk_size() -> NonZeroUsize {
    match NonZeroUsize::new(DEFAULT_CHUNK_SIZE) {
        Some(size) => size,
        None => NonZeroUsize::MIN,
    }
}

/// Collects every chunk a finished codec produces.
fn drain(mut pull: impl FnMut() -> Result<crate::Output>) -> Result<BytesView> {
    // After finishing, the codec never asks for more input, so `into_data` returning `None` means
    // the stream ended.
    let mut parts = Vec::new();
    while let Some(chunk) = pull()?.into_data() {
        parts.push(chunk);
    }

    Ok(BytesView::from_views(parts))
}

/// Configures an encoder for a [`Format`] chosen at runtime.
///
/// Mirrors the per-format builders such as [`gzip::EncoderBuilder`][crate::gzip::EncoderBuilder],
/// but produces a boxed [`Encoder`] so the format need not be known at compile time.
#[derive(Debug, Clone, Copy)]
pub struct EncoderBuilder {
    format: Format,
    level: Level,
    chunk_size: NonZeroUsize,
}

impl EncoderBuilder {
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

    /// Builds the encoder, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Box<dyn Encoder> {
        macro_rules! build {
            ($module:ident) => {
                Box::new(
                    crate::$module::Encoder::builder()
                        .level(self.level)
                        .output_chunk_size(self.chunk_size)
                        .build(memory),
                )
            };
        }

        match self.format {
            Format::Deflate => build!(deflate),
            Format::Zlib => build!(zlib),
            Format::Gzip => build!(gzip),
            #[cfg(feature = "brotli")]
            Format::Brotli => build!(brotli),
        }
    }
}

/// Configures a decoder for a [`Format`] chosen at runtime.
///
/// Mirrors the per-format builders such as [`gzip::DecoderBuilder`][crate::gzip::DecoderBuilder],
/// but produces a boxed [`Decoder`] so the format need not be known at compile time.
#[derive(Debug, Clone, Copy)]
pub struct DecoderBuilder {
    format: Format,
    limits: DecompressionLimits,
    chunk_size: NonZeroUsize,
    concatenated: Option<bool>,
}

impl DecoderBuilder {
    /// Sets the bounds on how much data decompression may produce.
    ///
    /// # Security
    ///
    /// Tighten these beyond [`DecompressionLimits::DEFAULT`] when the data comes from an untrusted
    /// peer.
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

    /// Sets whether consecutive streams decode as one logical stream.
    ///
    /// Left unset, each format keeps its own default: enabled for [`Format::Gzip`], matching
    /// `gzip(1)`, and disabled for the others, where concatenation is not an established
    /// convention.
    #[must_use]
    pub const fn concatenated(mut self, enabled: bool) -> Self {
        self.concatenated = Some(enabled);
        self
    }

    /// Builds the decoder, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Box<dyn Decoder> {
        macro_rules! build {
            ($module:ident) => {{
                let builder = crate::$module::Decoder::builder()
                    .limits(self.limits)
                    .output_chunk_size(self.chunk_size);

                let builder = match self.concatenated {
                    Some(enabled) => builder.concatenated(enabled),
                    None => builder,
                };

                Box::new(builder.build(memory))
            }};
        }

        match self.format {
            Format::Deflate => build!(deflate),
            Format::Zlib => build!(zlib),
            Format::Gzip => build!(gzip),
            #[cfg(feature = "brotli")]
            Format::Brotli => build!(brotli),
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

    fn encoded_len(builder: EncoderBuilder, payload: &[u8]) -> usize {
        let mut encoder = builder.build(GlobalPool::new());
        encoder.push(view(payload)).expect("push succeeds");
        encoder.finish();

        let mut total = 0;
        while let Some(chunk) = encoder.pull().expect("pull succeeds").into_data() {
            total += chunk.len();
        }

        total
    }

    #[test]
    fn every_format_round_trips_through_the_enum() {
        let payload = b"runtime selected format ".repeat(200);

        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let encoded = format.compress(view(&payload), memory.clone()).expect("compression succeeds");
            let plain = format.decompress(encoded, memory).expect("decompression succeeds");

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

    #[test]
    fn http_deflate_token_means_zlib() {
        // The most common source of confusion in this area: HTTP's `deflate` token denotes a zlib
        // stream, not raw deflate.
        assert_eq!(Format::from_content_encoding("deflate"), Some(Format::Zlib));
        assert_eq!(Format::Deflate.content_encoding(), None);
    }

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

    #[test]
    fn the_encoder_builder_applies_its_level() {
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(400);

        for &format in Format::ALL {
            let fast = encoded_len(format.encoder().level(Level::FAST), &payload);
            let best = encoded_len(format.encoder().level(Level::BEST), &payload);

            assert!(best <= fast, "{format:?}: best={best} should not exceed fast={fast}");
        }
    }

    #[test]
    fn the_encoder_builder_applies_its_chunk_size() {
        let bound = NonZeroUsize::new(128).expect("128 is not zero");

        for &format in Format::ALL {
            let mut encoder = format.encoder().output_chunk_size(bound).build(GlobalPool::new());
            encoder.push(view(&b"chunked ".repeat(5_000))).expect("push succeeds");
            encoder.finish();

            while let Some(chunk) = encoder.pull().expect("pull succeeds").into_data() {
                assert!(chunk.len() <= bound.get(), "{format:?} produced a {} byte chunk", chunk.len());
            }
        }
    }

    #[test]
    fn the_decoder_builder_applies_its_limits() {
        for &format in Format::ALL {
            let memory = GlobalPool::new();
            let encoded = format
                .compress(view(&vec![0_u8; 4 * 1024 * 1024]), memory.clone())
                .expect("compression succeeds");

            let mut decoder = format
                .decoder()
                .limits(DecompressionLimits::DEFAULT.with_max_output_len(1024))
                .build(memory);
            decoder.push(encoded).expect("push succeeds");
            decoder.finish();

            let error = loop {
                match decoder.pull() {
                    Ok(Output::Data(_)) => {}
                    Ok(_) => panic!("{format:?}: the cap should have fired"),
                    Err(error) => break error,
                }
            };

            assert!(error.is_limit_exceeded(), "{format:?}: got {error}");
        }
    }

    #[test]
    fn the_decoder_builder_keeps_each_formats_concatenation_default() {
        // Gzip decodes concatenated members by default; the others stop at the first stream. The
        // runtime builder must preserve that rather than flattening every format to one behaviour.
        let memory = GlobalPool::new();
        let payload = b"member ".repeat(50);

        let encoded = Format::Gzip.compress(view(&payload), memory.clone()).expect("compress");
        let joined = BytesView::from_views([encoded.clone(), encoded]);

        let joined_len = Format::Gzip.decompress(joined.clone(), memory.clone()).expect("decompress").len();
        assert_eq!(joined_len, payload.len() * 2, "gzip should join members by default");

        let mut decoder = Format::Gzip.decoder().concatenated(false).build(memory);
        decoder.push(joined).expect("push succeeds");
        decoder.finish();

        let mut total = 0;
        while let Some(chunk) = decoder.pull().expect("pull succeeds").into_data() {
            total += chunk.len();
        }

        assert_eq!(total, payload.len(), "disabling concatenation should stop after one member");
    }

    #[test]
    fn all_lists_exactly_the_compiled_in_formats() {
        let expected = if cfg!(feature = "brotli") { 4 } else { 3 };

        assert_eq!(Format::ALL.len(), expected);
    }
}
