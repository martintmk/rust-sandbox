// Licensed under the MIT License.

use std::mem::MaybeUninit;
use std::num::NonZeroUsize;

use bytesbuf::BytesView;
use bytesbuf::mem::MemoryShared;
use flate2::{Decompress, FlushDecompress, Status};

use crate::engine::{Codec, DEFAULT_CHUNK_SIZE, Pump, Step};
use crate::error::{Error, Result};
use crate::gzip::encoder::WINDOW_BITS;
use crate::limits::DecompressionLimits;
use crate::output::Output;

#[derive(Debug)]
struct DecompressCodec {
    decompress: Decompress,
    limits: DecompressionLimits,
    multi_member: bool,
}

impl Codec for DecompressCodec {
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], _last_input: bool) -> Result<(Step, usize, usize)> {
        let before_in = self.decompress.total_in();
        let before_out = self.decompress.total_out();

        let status = self
            .decompress
            .decompress_uninit(input, output, FlushDecompress::None)
            .map_err(|error| Error::corrupt_data("the compressed data is not a valid gzip stream").with_source(error))?;

        let consumed = usize::try_from(self.decompress.total_in() - before_in).unwrap_or(usize::MAX);
        let produced = usize::try_from(self.decompress.total_out() - before_out).unwrap_or(usize::MAX);

        let step = if status == Status::StreamEnd {
            Step::StreamEnd
        } else {
            Step::Continue
        };

        Ok((step, consumed, produced))
    }

    fn stream_ended(&mut self, more_input_available: bool) -> bool {
        if !self.multi_member || !more_input_available {
            return true;
        }

        // A concatenated member follows. The engine must be replaced rather than reset:
        // `Decompress::reset` takes a `bool` that selects between raw deflate and zlib, and so
        // cannot express gzip framing (which the engine encodes as `window_bits + 16`). Resetting
        // silently drops the decoder to raw deflate, and the next member then fails with
        // "invalid block type".
        self.decompress = Decompress::new_gzip(WINDOW_BITS);
        false
    }

    fn check_limits(&self, total_in: u64, total_out: u64) -> Result<()> {
        self.limits.check(total_in, total_out)
    }
}

/// Decompresses a gzip stream into a stream of [`BytesView`] values.
///
/// Like [`Encoder`][crate::Encoder], this is a push/pull state machine, so decompressing a
/// multi-gigabyte stream never buffers more than one chunk of output at a time.
///
/// # Security
///
/// Gzip can expand its input by orders of magnitude, so a decoder pointed at untrusted data is a
/// memory-exhaustion vector. [`DecompressionLimits::DEFAULT`] caps expansion at 1000x; tighten it
/// with [`DecoderBuilder::limits`] when the source is untrusted.
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::GlobalPool;
/// use compressed::gzip;
///
/// let memory = GlobalPool::new();
/// let compressed = gzip::compress(
///     BytesView::copied_from_slice(b"round trip", &memory),
///     memory.clone(),
/// )?;
///
/// let plain = gzip::decompress(compressed, memory)?;
/// assert_eq!(plain.to_vec(), b"round trip".to_vec());
/// # Ok::<(), compressed::Error>(())
/// ```
#[derive(Debug)]
pub struct Decoder {
    pump: Pump,
    codec: DecompressCodec,
}

impl Decoder {
    /// Creates a decoder with [`DecompressionLimits::DEFAULT`].
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
    /// Returns an [`Error::is_invalid_state`] error if input is still pending from a previous
    /// push, or if [`Decoder::finish`] has already been called.
    pub fn push(&mut self, input: BytesView) -> Result<()> {
        self.pump.push(input)
    }

    /// Signals that no further input will be supplied.
    ///
    /// Calling this more than once has no additional effect. If the stream ended part-way through
    /// a gzip member, the next [`Decoder::pull`] reports
    /// [`Error::is_unexpected_end_of_stream`].
    pub fn finish(&mut self) {
        self.pump.finish();
    }

    /// Produces the next chunk of decompressed output.
    ///
    /// # Errors
    ///
    /// Returns [`Error::is_corrupt_data`] if the input is not valid gzip or its checksum does not
    /// match, [`Error::is_limit_exceeded`] if the configured limits would be exceeded, or
    /// [`Error::is_unexpected_end_of_stream`] if the input ended mid-member.
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
#[derive(Debug, Clone, Copy)]
pub struct DecoderBuilder {
    limits: DecompressionLimits,
    chunk_size: NonZeroUsize,
    multi_member: bool,
}

impl DecoderBuilder {
    /// Sets the bounds on how much data decompression may produce.
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

    /// Sets whether concatenated gzip members decode as one logical stream.
    ///
    /// Enabled by default, matching `gzip(1)`. When enabled, any bytes following a member must
    /// themselves form a valid member; trailing padding is rejected as corrupt data. Disable this
    /// to stop after the first member and ignore whatever follows.
    #[must_use]
    pub const fn multi_member(mut self, enabled: bool) -> Self {
        self.multi_member = enabled;
        self
    }

    /// Builds the decoder, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Decoder {
        Decoder {
            pump: Pump::new(memory, self.chunk_size),
            codec: DecompressCodec {
                decompress: Decompress::new_gzip(WINDOW_BITS),
                limits: self.limits,
                multi_member: self.multi_member,
            },
        }
    }
}

impl Default for DecoderBuilder {
    fn default() -> Self {
        Self {
            limits: DecompressionLimits::DEFAULT,
            chunk_size: NonZeroUsize::new(DEFAULT_CHUNK_SIZE).unwrap_or(NonZeroUsize::MIN),
            multi_member: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    fn gzip(bytes: &[u8]) -> BytesView {
        crate::gzip::compress(view(bytes), GlobalPool::new()).expect("compression succeeds")
    }

    #[test]
    fn round_trips_a_payload() {
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(500);
        let plain = crate::gzip::decompress(gzip(&payload), GlobalPool::new()).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), payload);
    }

    #[test]
    fn round_trips_empty_input() {
        let plain = crate::gzip::decompress(gzip(b""), GlobalPool::new()).expect("decompression succeeds");

        assert!(plain.is_empty());
    }

    #[test]
    fn decodes_concatenated_members_as_one_stream() {
        // Regression guard: `Decompress::reset` cannot express gzip framing, so the decoder must
        // build a fresh engine per member. With a reset the second member fails with
        // "invalid block type".
        let member = gzip(b"member payload ");
        let joined = BytesView::from_views([member.clone(), member.clone(), member]);

        let plain = crate::gzip::decompress(joined, GlobalPool::new()).expect("decompression succeeds");

        assert_eq!(plain.to_vec(), b"member payload ".repeat(3));
    }

    #[test]
    fn stops_at_the_first_member_when_multi_member_is_disabled() {
        let member = gzip(b"first");
        let joined = BytesView::from_views([member.clone(), member]);

        let mut decoder = Decoder::builder().multi_member(false).build(GlobalPool::new());
        decoder.push(joined).expect("push succeeds");
        decoder.finish();

        let mut plain = Vec::new();
        while let Some(chunk) = decoder.pull().expect("pull succeeds").into_data() {
            plain.extend_from_slice(&chunk.to_vec());
        }

        assert_eq!(plain, b"first".to_vec());
    }

    #[test]
    fn rejects_a_truncated_stream() {
        let full = gzip(&b"payload that spans a few blocks ".repeat(100));
        let truncated = full.range(0..full.len() - 8);

        let error = crate::gzip::decompress(truncated, GlobalPool::new()).expect_err("truncation is detected");

        assert!(error.is_unexpected_end_of_stream(), "got {error}");
    }

    #[test]
    fn rejects_data_that_is_not_gzip() {
        let error = crate::gzip::decompress(view(b"not gzip at all, just text"), GlobalPool::new()).expect_err("bad data is rejected");

        assert!(error.is_corrupt_data(), "got {error}");
    }

    #[test]
    fn rejects_a_corrupted_checksum() {
        let full = gzip(b"checksummed payload");
        let mut bytes = full.to_vec();

        // The CRC32 occupies the four bytes before the ISIZE trailer.
        let crc_start = bytes.len() - 8;
        bytes[crc_start] ^= 0xff;

        let error = crate::gzip::decompress(view(&bytes), GlobalPool::new()).expect_err("a bad checksum is rejected");

        assert!(error.is_corrupt_data(), "got {error}");
    }

    #[test]
    fn rejects_a_corrupted_length_trailer() {
        let full = gzip(b"length checked payload");
        let mut bytes = full.to_vec();

        let isize_start = bytes.len() - 4;
        bytes[isize_start] ^= 0xff;

        let error = crate::gzip::decompress(view(&bytes), GlobalPool::new()).expect_err("a bad length is rejected");

        assert!(error.is_corrupt_data(), "got {error}");
    }

    #[test]
    fn enforces_the_expansion_limit() {
        // Highly compressible input expands far beyond the default ratio.
        let bomb = gzip(&vec![0_u8; 8 * 1024 * 1024]);

        let mut decoder = Decoder::new(GlobalPool::new());
        decoder.push(bomb).expect("push succeeds");
        decoder.finish();

        let error = loop {
            match decoder.pull() {
                Ok(Output::Data(_)) => {}
                Ok(_) => break None,
                Err(failure) => break Some(failure),
            }
        };

        let error = error.expect("the bomb guard fires");
        assert!(error.is_limit_exceeded(), "got {error}");
    }

    #[test]
    fn unlimited_accepts_what_the_default_rejects() {
        let payload = vec![0_u8; 8 * 1024 * 1024];
        let bomb = gzip(&payload);

        let mut decoder = Decoder::builder().limits(DecompressionLimits::UNLIMITED).build(GlobalPool::new());
        decoder.push(bomb).expect("push succeeds");
        decoder.finish();

        let mut total = 0;
        while let Some(chunk) = decoder.pull().expect("pull succeeds").into_data() {
            total += chunk.len();
        }

        assert_eq!(total, payload.len());
    }

    #[test]
    fn absolute_limit_is_enforced() {
        let gzip = gzip(&b"x".repeat(1024 * 1024));

        let mut decoder = Decoder::builder()
            .limits(DecompressionLimits::UNLIMITED.with_max_output_len(1024))
            .build(GlobalPool::new());
        decoder.push(gzip).expect("push succeeds");
        decoder.finish();

        let error = loop {
            match decoder.pull() {
                Ok(Output::Data(_)) => {}
                Ok(_) => break None,
                Err(failure) => break Some(failure),
            }
        };

        assert!(error.expect("the cap fires").is_limit_exceeded());
    }

    #[test]
    fn tracks_byte_counts() {
        let payload = b"counted payload".repeat(100);
        let compressed = gzip(&payload);
        let compressed_len = compressed.len();

        let mut decoder = Decoder::new(GlobalPool::new());
        decoder.push(compressed).expect("push succeeds");
        decoder.finish();

        while decoder.pull().expect("pull succeeds").into_data().is_some() {}

        assert_eq!(decoder.total_in(), compressed_len as u64);
        assert_eq!(decoder.total_out(), payload.len() as u64);
    }

    #[test]
    fn rejects_input_after_finish() {
        let mut decoder = Decoder::new(GlobalPool::new());
        decoder.finish();

        let error = decoder.push(view(b"late")).expect_err("push after finish is rejected");
        assert!(error.is_invalid_state());
    }

    #[test]
    fn reports_need_input_before_finish() {
        let mut decoder = Decoder::new(GlobalPool::new());
        let compressed = gzip(&b"partial".repeat(50));

        // Feed only the first half, so the decoder cannot complete the member.
        decoder.push(compressed.range(0..compressed.len() / 2)).expect("push succeeds");

        let output = loop {
            match decoder.pull().expect("pull succeeds") {
                Output::Data(_) => {}
                other => break other,
            }
        };

        assert!(output.is_need_input());
    }

    #[test]
    fn builder_defaults_are_safe() {
        let builder = DecoderBuilder::default();

        assert_eq!(builder.limits, DecompressionLimits::DEFAULT);
        assert!(builder.multi_member, "concatenated members decode by default, like gzip(1)");
        assert_eq!(builder.chunk_size.get(), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn honours_the_output_chunk_size() {
        let chunk = NonZeroUsize::new(100).expect("100 is not zero");
        let compressed = gzip(&b"chunked output ".repeat(5_000));

        let mut decoder = Decoder::builder().output_chunk_size(chunk).build(GlobalPool::new());
        decoder.push(compressed).expect("push succeeds");
        decoder.finish();

        let first = decoder.pull().expect("pull succeeds").into_data().expect("output is available");

        assert!(first.len() <= 200, "chunk was {} bytes, expected around 100", first.len());
    }
}
