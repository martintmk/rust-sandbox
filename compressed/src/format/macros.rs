// Licensed under the MIT License.

//! The macro that generates each format module's public surface.
//!
//! Every format exposes the same four types and two functions, differing only in which codec they
//! drive and in their documentation. Generating them keeps the four modules honest — a change to
//! the contract cannot drift between formats — without collapsing them into one type that would
//! lose the compile-time distinction between, say, a gzip and a brotli encoder.
//!
//! # Format-specific settings
//!
//! Formats are not actually identical: gzip carries optional header metadata, brotli has a window
//! size and a content mode, zlib takes a preset dictionary. The macro handles that with an
//! `encoder_options` / `decoder_options` type, defaulted and threaded through to the codec. A
//! format with no extra settings passes `()`; a format that has some declares its own options
//! struct and writes the setters by hand in its own module, right next to the documentation that
//! explains them.
//!
//! Only the portable settings appear on the runtime [`Format`][crate::Format] builders, because a
//! builder that might produce any format cannot honour a setting that only one of them has. Code
//! that needs both a runtime format and a format-specific setting branches on the format, uses the
//! concrete builder, and boxes the result — which works because `Box<dyn Encoder>` is itself an
//! [`Encoder`][crate::Encoder].

/// Generates `Encoder`, `EncoderBuilder`, `Decoder`, `DecoderBuilder`, `compress` and `decompress`
/// for one format.
macro_rules! define_format {
    (
        name = $name:literal,
        encoder_codec = $enc_codec:ty,
        encoder_options = $enc_options:ty,
        new_encoder = $new_encoder:expr,
        decoder_codec = $dec_codec:ty,
        decoder_options = $dec_options:ty,
        default_limits = $default_limits:expr,
        new_decoder = $new_decoder:expr,
        concatenated_default = $concatenated_default:expr,
        concatenated_doc = $concatenated_doc:literal,
    ) => {
        use std::num::NonZeroUsize;

        use bytesbuf::BytesView;
        use bytesbuf::mem::MemoryShared;
        // Anonymous, because this module defines its own `Encoder` and `Decoder` types; the
        // imports exist only to bring the traits' provided methods into scope.
        use $crate::codec::{Decoder as _, Encoder as _};
        use $crate::engine::{DEFAULT_CHUNK_SIZE, Pump};
        use $crate::error::Result;
        use $crate::level::Level;
        use $crate::limits::DecompressionLimits;
        use $crate::output::Output;

        #[doc = concat!("Compresses a stream of byte sequences into ", $name, ".")]
        ///
        /// A push/pull state machine: supply input with [`Encoder::push`], take output with
        /// [`Encoder::pull`], and call [`Encoder::finish`] when there is no more input. Each pull
        /// returns at most one bounded chunk, so a stream of any length can be compressed with a
        /// bounded working set.
        #[derive(Debug)]
        pub struct Encoder {
            pump: Pump,
            codec: $enc_codec,
        }

        impl Encoder {
            /// Creates an encoder at [`Level::DEFAULT`].
            #[must_use]
            pub fn new(memory: impl MemoryShared) -> Self {
                Self::builder().build(memory)
            }

            /// Starts configuring an encoder.
            #[must_use]
            pub fn builder() -> EncoderBuilder {
                EncoderBuilder::default()
            }

            /// Supplies more uncompressed input.
            ///
            /// # Errors
            ///
            /// Returns an [`Error::is_invalid_state`][crate::Error::is_invalid_state] error if
            /// input is still pending from a previous push, or if [`Encoder::finish`] has already
            /// been called. Drain pending input with [`Encoder::pull`] until it reports
            /// [`Output::NeedInput`] first.
            pub fn push(&mut self, input: BytesView) -> Result<()> {
                self.pump.push(input)
            }

            /// Signals that no further input will be supplied.
            ///
            /// Calling this more than once has no additional effect.
            pub fn finish(&mut self) {
                self.pump.finish();
            }

            /// Produces the next chunk of compressed output.
            ///
            /// # Errors
            ///
            /// Returns an error if the underlying compression engine fails.
            pub fn pull(&mut self) -> Result<Output> {
                self.pump.pull(&mut self.codec)
            }

            /// The number of uncompressed bytes consumed so far.
            #[must_use]
            pub fn total_in(&self) -> u64 {
                self.pump.total_in()
            }

            /// The number of compressed bytes produced so far.
            #[must_use]
            pub fn total_out(&self) -> u64 {
                self.pump.total_out()
            }
        }

        /// Configures an [`Encoder`].
        #[derive(Debug, Clone)]
        pub struct EncoderBuilder {
            level: Level,
            chunk_size: NonZeroUsize,
            pool: Option<$crate::Pool>,
            /// Settings that only this format has. `()` for formats with none.
            ///
            /// The generated builder never reads this beyond handing it to the codec; the format's
            /// own module adds the setters that populate it.
            options: $enc_options,
        }

        impl EncoderBuilder {
            #[doc = concat!("Sets the compression level, mapped onto ", $name, "'s native range.")]
            #[must_use]
            pub const fn level(mut self, level: Level) -> Self {
                self.level = level;
                self
            }

            /// Sets how much output a single [`Encoder::pull`] produces before returning.
            ///
            /// This bounds the encoder's working set. Larger chunks reduce per-call overhead;
            /// smaller chunks reduce peak memory and latency.
            #[must_use]
            pub const fn output_chunk_size(mut self, bytes: NonZeroUsize) -> Self {
                self.chunk_size = bytes;
                self
            }

            /// Recycles engine state through a shared [`Pool`][crate::Pool].
            ///
            /// Building a compressor is not free, so a service that encodes many messages should
            /// hand every encoder the same pool. The engine is returned when the encoder is
            /// dropped. Without a pool each encoder builds its own engine, which is the default.
            #[must_use]
            pub fn pool(mut self, pool: $crate::Pool) -> Self {
                self.pool = Some(pool);
                self
            }

            /// Builds the encoder, drawing its output buffers from `memory`.
            #[must_use]
            pub fn build(self, memory: impl MemoryShared) -> Encoder {
                Encoder {
                    pump: Pump::new(memory, self.chunk_size),
                    codec: $new_encoder(self.level, self.options, self.pool),
                }
            }
        }

        impl Default for EncoderBuilder {
            fn default() -> Self {
                Self {
                    level: Level::DEFAULT,
                    chunk_size: NonZeroUsize::new(DEFAULT_CHUNK_SIZE).unwrap_or(NonZeroUsize::MIN),
                    pool: None,
                    options: <$enc_options>::default(),
                }
            }
        }

        #[doc = concat!("Decompresses a ", $name, " stream into a stream of byte sequences.")]
        ///
        /// # Security
        ///
        /// Compressed data can expand by orders of magnitude, so a decoder pointed at untrusted
        /// input is a memory-exhaustion vector. This format's own default bounds apply unless
        /// [`DecoderBuilder::limits`] overrides them.
        #[derive(Debug)]
        pub struct Decoder {
            pump: Pump,
            codec: $dec_codec,
        }

        impl Decoder {
            #[must_use]
            pub fn new(memory: impl MemoryShared) -> Self {
                Self::builder().build(memory)
            }

            /// Starts configuring a decoder.
            #[must_use]
            pub fn builder() -> DecoderBuilder {
                DecoderBuilder::default()
            }

            /// Supplies more compressed input.
            ///
            /// # Errors
            ///
            /// Returns an [`Error::is_invalid_state`][crate::Error::is_invalid_state] error if
            /// input is still pending from a previous push, or if [`Decoder::finish`] has already
            /// been called.
            pub fn push(&mut self, input: BytesView) -> Result<()> {
                self.pump.push(input)
            }

            /// Signals that no further input will be supplied.
            ///
            /// If the input ended part-way through a stream, the next [`Decoder::pull`] reports
            /// [`Error::is_unexpected_end_of_stream`][crate::Error::is_unexpected_end_of_stream].
            pub fn finish(&mut self) {
                self.pump.finish();
            }

            /// Produces the next chunk of decompressed output.
            ///
            /// # Errors
            ///
            /// Returns [`Error::is_corrupt_data`][crate::Error::is_corrupt_data] if the input is
            /// malformed, [`Error::is_limit_exceeded`][crate::Error::is_limit_exceeded] if the
            /// configured limits would be exceeded, or
            /// [`Error::is_unexpected_end_of_stream`][crate::Error::is_unexpected_end_of_stream]
            /// if the input ended early.
            pub fn pull(&mut self) -> Result<Output> {
                self.pump.pull(&mut self.codec)
            }

            /// The number of compressed bytes consumed so far.
            #[must_use]
            pub fn total_in(&self) -> u64 {
                self.pump.total_in()
            }

            /// The number of decompressed bytes produced so far.
            #[must_use]
            pub fn total_out(&self) -> u64 {
                self.pump.total_out()
            }
        }

        /// Configures a [`Decoder`].
        #[derive(Debug, Clone)]
        pub struct DecoderBuilder {
            limits: DecompressionLimits,
            chunk_size: NonZeroUsize,
            concatenated: bool,
            pool: Option<$crate::Pool>,
            /// Settings that only this format has. `()` for formats with none.
            options: $dec_options,
        }

        impl DecoderBuilder {
            #[doc = concat!("Overrides the bounds on how much data decompression may produce.")]
            ///
            /// Bounds left unset on the passed value keep this format's own defaults.
            #[must_use]
            pub const fn limits(mut self, limits: DecompressionLimits) -> Self {
                self.limits = limits;
                self
            }

            /// Sets how much output a single [`Decoder::pull`] produces before returning.
            #[must_use]
            pub const fn output_chunk_size(mut self, bytes: NonZeroUsize) -> Self {
                self.chunk_size = bytes;
                self
            }

            #[doc = $concatenated_doc]
            ///
            /// When enabled, any bytes following a complete stream must themselves form another
            /// valid stream; trailing padding is reported as corrupt data. Disable this to stop
            /// after the first stream and ignore whatever follows, using
            /// [`Decoder::total_in`] to find where it ended.
            #[must_use]
            pub const fn concatenated(mut self, enabled: bool) -> Self {
                self.concatenated = enabled;
                self
            }

            /// Recycles engine state through a shared [`Pool`][crate::Pool].
            ///
            /// The engine is returned when the decoder is dropped. Without a pool each decoder
            /// builds its own engine, which is the default. See [`Pool`][crate::Pool] for which
            /// engines are actually recycled.
            #[must_use]
            pub fn pool(mut self, pool: $crate::Pool) -> Self {
                self.pool = Some(pool);
                self
            }

            /// Builds the decoder, drawing its output buffers from `memory`.
            #[must_use]
            pub fn build(self, memory: impl MemoryShared) -> Decoder {
                Decoder {
                    pump: Pump::new(memory, self.chunk_size),
                    codec: $new_decoder(
                        self.limits.resolve($default_limits),
                        self.concatenated,
                        self.options,
                        self.pool,
                    ),
                }
            }
        }

        impl Default for DecoderBuilder {
            fn default() -> Self {
                Self {
                    limits: DecompressionLimits::new(),
                    chunk_size: NonZeroUsize::new(DEFAULT_CHUNK_SIZE).unwrap_or(NonZeroUsize::MIN),
                    concatenated: $concatenated_default,
                    pool: None,
                    options: <$dec_options>::default(),
                }
            }
        }

        #[doc = concat!("Compresses a complete byte sequence into ", $name, ".")]
        ///
        /// Uses [`Level::DEFAULT`]. Prefer [`Encoder`] for data that arrives incrementally; this
        /// convenience buffers the entire result before returning.
        ///
        /// # Errors
        ///
        /// Returns an error if the underlying compression engine fails.
        pub fn compress(input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
            Encoder::new(memory).encode(input)
        }

        #[doc = concat!("Decompresses a complete ", $name, " stream that is already in memory.")]
        ///
        /// Applies this format's default bounds. Prefer [`Decoder`] for data that arrives
        /// incrementally; this convenience buffers the entire result before returning.
        ///
        /// # Errors
        ///
        /// Returns an error if the data is malformed, truncated, or exceeds the default limits.
        pub fn decompress(input: BytesView, memory: impl MemoryShared) -> Result<BytesView> {
            Decoder::new(memory).decode(input)
        }
    };
}

pub(crate) use define_format;
