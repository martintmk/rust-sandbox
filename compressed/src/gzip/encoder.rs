// Licensed under the MIT License.

use std::mem::MaybeUninit;
use std::num::NonZeroUsize;

use bytesbuf::BytesView;
use bytesbuf::mem::MemoryShared;
use flate2::{Compress, Compression, FlushCompress, Status};

use crate::engine::{Codec, DEFAULT_CHUNK_SIZE, Pump, Step};
use crate::error::{Error, Result};
use crate::level::Level;
use crate::output::Output;

/// The deflate window size exponent. 15 is the maximum, giving the best compression ratio.
pub(crate) const WINDOW_BITS: u8 = 15;

#[derive(Debug)]
struct CompressCodec {
    compress: Compress,
}

impl Codec for CompressCodec {
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], last_input: bool) -> Result<(Step, usize, usize)> {
        let flush = if last_input { FlushCompress::Finish } else { FlushCompress::None };

        let before_in = self.compress.total_in();
        let before_out = self.compress.total_out();

        let status = self
            .compress
            .compress_uninit(input, output, flush)
            .map_err(|error| Error::invalid_state("the compression engine reported a failure").with_source(error))?;

        let consumed = usize::try_from(self.compress.total_in() - before_in).unwrap_or(usize::MAX);
        let produced = usize::try_from(self.compress.total_out() - before_out).unwrap_or(usize::MAX);

        let step = if status == Status::StreamEnd {
            Step::StreamEnd
        } else {
            Step::Continue
        };

        Ok((step, consumed, produced))
    }

    fn stream_ended(&mut self, _more_input_available: bool) -> bool {
        true
    }
}

/// Compresses a stream of [`BytesView`] values into gzip.
///
/// The encoder is a push/pull state machine rather than a single `compress(view) -> view` call, so
/// that its working set stays bounded no matter how long the stream is: it holds at most one
/// pending input view plus one chunk of output.
///
/// ```
/// use bytesbuf::BytesView;
/// use bytesbuf::mem::GlobalPool;
/// use compressed::gzip;
///
/// let memory = GlobalPool::new();
/// let mut encoder = gzip::Encoder::new(memory.clone());
/// let mut compressed = Vec::new();
///
/// encoder.push(BytesView::copied_from_slice(b"streamed payload", &memory))?;
/// encoder.finish();
///
/// while let Some(chunk) = encoder.pull()?.into_data() {
///     compressed.push(chunk);
/// }
///
/// let gzip = BytesView::from_views(compressed);
/// assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
/// # Ok::<(), compressed::Error>(())
/// ```
#[derive(Debug)]
pub struct Encoder {
    pump: Pump,
    codec: CompressCodec,
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
    /// Returns an [`Error::is_invalid_state`] error if input is still pending from a previous
    /// push, or if [`Encoder::finish`] has already been called. Drain the previously pushed
    /// input with [`Encoder::pull`] until it reports [`Output::NeedInput`] first.
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
#[derive(Debug, Clone, Copy)]
pub struct EncoderBuilder {
    level: Level,
    chunk_size: NonZeroUsize,
}

impl EncoderBuilder {
    /// Sets the compression level.
    #[must_use]
    pub const fn level(mut self, level: Level) -> Self {
        self.level = level;
        self
    }

    /// Sets how much output a single [`Encoder::pull`] produces before returning.
    ///
    /// This bounds the encoder's working set. Larger chunks reduce per-call overhead; smaller
    /// chunks reduce peak memory and latency.
    #[must_use]
    pub const fn output_chunk_size(mut self, bytes: NonZeroUsize) -> Self {
        self.chunk_size = bytes;
        self
    }

    /// Builds the encoder, drawing its output buffers from `memory`.
    #[must_use]
    pub fn build(self, memory: impl MemoryShared) -> Encoder {
        Encoder {
            pump: Pump::new(memory, self.chunk_size),
            codec: CompressCodec {
                compress: Compress::new_gzip(Compression::new(u32::from(self.level.get())), WINDOW_BITS),
            },
        }
    }
}

impl Default for EncoderBuilder {
    fn default() -> Self {
        Self {
            level: Level::DEFAULT,
            chunk_size: NonZeroUsize::new(DEFAULT_CHUNK_SIZE).unwrap_or(NonZeroUsize::MIN),
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

    #[test]
    fn produces_a_gzip_container() {
        let gzip = crate::gzip::compress(view(b"payload"), GlobalPool::new()).expect("compression succeeds");

        assert_eq!(gzip.range(0..3).to_vec(), vec![0x1f, 0x8b, 0x08], "expected a gzip header");
    }

    #[test]
    fn compresses_empty_input_into_a_valid_member() {
        let gzip = crate::gzip::compress(BytesView::new(), GlobalPool::new()).expect("compression succeeds");

        assert_eq!(gzip.range(0..2).to_vec(), vec![0x1f, 0x8b]);
        assert!(gzip.len() >= 18, "a gzip member carries 18 bytes of framing, got {}", gzip.len());
    }

    #[test]
    fn tracks_byte_counts() {
        let mut encoder = Encoder::new(GlobalPool::new());
        encoder.push(view(b"0123456789")).expect("push succeeds");
        encoder.finish();

        while encoder.pull().expect("pull succeeds").into_data().is_some() {}

        assert_eq!(encoder.total_in(), 10);
        assert!(encoder.total_out() > 0);
    }

    #[test]
    fn higher_levels_compress_at_least_as_well() {
        let payload = b"the quick brown fox jumps over the lazy dog ".repeat(200);

        let fast = Encoder::builder().level(Level::FAST).build(GlobalPool::new());
        let best = Encoder::builder().level(Level::BEST).build(GlobalPool::new());

        let fast_len = drain(fast, &payload);
        let best_len = drain(best, &payload);

        assert!(best_len <= fast_len, "best={best_len} should not exceed fast={fast_len}");
    }

    #[test]
    fn level_none_still_produces_a_valid_container() {
        let encoder = Encoder::builder().level(Level::NONE).build(GlobalPool::new());
        let payload = b"stored, not compressed".repeat(10);

        assert!(drain(encoder, &payload) > payload.len(), "stored data grows slightly");
    }

    #[test]
    fn honours_the_output_chunk_size() {
        let chunk = NonZeroUsize::new(64).expect("64 is not zero");
        let mut encoder = Encoder::builder().output_chunk_size(chunk).build(GlobalPool::new());

        encoder.push(view(&b"compressible ".repeat(5_000))).expect("push succeeds");
        encoder.finish();

        let first = encoder.pull().expect("pull succeeds").into_data().expect("output is available");

        assert!(first.len() <= 128, "chunk was {} bytes, expected around 64", first.len());
    }

    #[test]
    fn rejects_input_after_finish() {
        let mut encoder = Encoder::new(GlobalPool::new());
        encoder.finish();

        let error = encoder.push(view(b"late")).expect_err("push after finish is rejected");
        assert!(error.is_invalid_state());
    }

    #[test]
    fn reports_need_input_before_finish() {
        let mut encoder = Encoder::new(GlobalPool::new());
        encoder.push(view(b"some data")).expect("push succeeds");

        // Drain until the encoder is hungry again.
        let output = loop {
            match encoder.pull().expect("pull succeeds") {
                Output::Data(_) => {}
                other => break other,
            }
        };

        assert!(output.is_need_input(), "encoder should ask for more input before finish");
    }

    #[test]
    fn builder_defaults_match_new() {
        let builder = EncoderBuilder::default();

        assert_eq!(builder.level, Level::DEFAULT);
        assert_eq!(builder.chunk_size.get(), DEFAULT_CHUNK_SIZE);
    }

    fn drain(mut encoder: Encoder, payload: &[u8]) -> usize {
        encoder.push(view(payload)).expect("push succeeds");
        encoder.finish();

        let mut total = 0;
        while let Some(chunk) = encoder.pull().expect("pull succeeds").into_data() {
            total += chunk.len();
        }

        total
    }
}
