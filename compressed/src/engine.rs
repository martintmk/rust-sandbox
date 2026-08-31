// Licensed under the MIT License.

use std::mem::MaybeUninit;
use std::num::NonZeroUsize;

use bytesbuf::mem::{MemoryShared, OpaqueMemory};
use bytesbuf::{BytesBuf, BytesView};

use crate::error::{Error, Result};
use crate::output::Output;

/// How much output a single `pull` produces before handing control back.
///
/// This bounds the codec's working set: a caller streaming hundreds of gigabytes never holds more
/// than one pending input view plus one chunk of output.
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

/// The outcome of a single engine step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The engine can do more work, given more input or more output space.
    Continue,
    /// The engine reached the end of a compressed stream.
    StreamEnd,
}

/// One direction of a compression algorithm, as the [`Pump`] drives it.
pub(crate) trait Codec {
    /// Runs a single engine step.
    ///
    /// Returns the step outcome, the number of input bytes consumed, and the number of output
    /// bytes written to the front of `output`.
    ///
    /// `last_input` is true only when `input` is the final slice the codec will ever receive. A
    /// [`BytesView`] is a chain of segments, so "the caller finished pushing" is not the same as
    /// "this is the last slice": finalizing on a non-final segment would truncate the stream.
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], last_input: bool) -> Result<(Step, usize, usize)>;

    /// Called when [`Codec::step`] reported [`Step::StreamEnd`].
    ///
    /// `more_input_available` says whether unconsumed input remains. Returns `true` if the logical
    /// stream is complete, or `false` if the codec re-armed itself for another container.
    fn stream_ended(&mut self, more_input_available: bool) -> bool;

    /// Validates the cumulative byte counts, for codecs that enforce limits.
    fn check_limits(&self, total_in: u64, total_out: u64) -> Result<()> {
        let _ = (total_in, total_out);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Accepting input.
    Open,
    /// The caller signalled end of input; drain the engine.
    Finishing,
    /// The engine reported end of stream.
    Done,
}

/// Moves bytes between a [`BytesView`] source and a [`BytesBuf`] sink through a [`Codec`].
///
/// This is where the impedance match happens: `BytesView` is a chain of segments with no
/// contiguous representation, and `BytesBuf` exposes its spare capacity one uninitialized segment
/// at a time. Both are fed to the engine a segment at a time, so no intermediate copy is needed
/// and no `std::io` trait is involved.
#[derive(Debug)]
pub(crate) struct Pump {
    memory: OpaqueMemory,
    chunk_size: usize,
    input: BytesView,
    output: BytesBuf,
    total_in: u64,
    total_out: u64,
    state: State,
}

impl Pump {
    pub(crate) fn new(memory: impl MemoryShared, chunk_size: NonZeroUsize) -> Self {
        let memory = OpaqueMemory::new(memory);
        let output = memory.reserve(chunk_size.get());

        Self {
            memory,
            chunk_size: chunk_size.get(),
            input: BytesView::new(),
            output,
            total_in: 0,
            total_out: 0,
            state: State::Open,
        }
    }

    pub(crate) fn push(&mut self, input: BytesView) -> Result<()> {
        if self.state != State::Open {
            return Err(Error::invalid_state("cannot push more input after the codec has been finished"));
        }

        if !self.input.is_empty() {
            return Err(Error::invalid_state(
                "cannot push more input while previously pushed input is still pending",
            ));
        }

        self.input = input;
        Ok(())
    }

    pub(crate) fn finish(&mut self) {
        if self.state == State::Open {
            self.state = State::Finishing;
        }
    }

    pub(crate) fn total_in(&self) -> u64 {
        self.total_in
    }

    pub(crate) fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Hands over whatever output has accumulated, if any.
    fn take_output(&mut self) -> Option<BytesView> {
        if self.output.is_empty() {
            return None;
        }

        Some(self.output.consume_all())
    }

    pub(crate) fn pull(&mut self, codec: &mut impl Codec) -> Result<Output> {
        if self.state == State::Done {
            return Ok(self.take_output().map_or(Output::Done, Output::Data));
        }

        loop {
            // Hand over a full chunk rather than growing the buffer, so the working set stays
            // bounded no matter how long the stream is.
            if self.output.len() >= self.chunk_size
                && let Some(data) = self.take_output()
            {
                return Ok(Output::Data(data));
            }

            let end_of_input = self.state == State::Finishing;

            if self.input.is_empty() && !end_of_input {
                return Ok(self.take_output().map_or(Output::NeedInput, Output::Data));
            }

            if self.output.remaining_capacity() == 0 {
                self.output.reserve(self.chunk_size, &self.memory);
            }

            // A memory provider may hand back more capacity than asked for, so the chunk bound has
            // to be applied to the slice itself rather than to the reservation. This also bounds
            // the cost of the engine's zero-fill of the uninitialized output slice.
            let budget = self.chunk_size - self.output.len();
            let pending = self.input.len();

            let (step, consumed, produced) = {
                let input = self.input.first_slice();
                let last_input = end_of_input && input.len() == pending;
                let spare = self.output.first_unfilled_slice();
                let take = spare.len().min(budget);
                codec.step(input, &mut spare[..take], last_input)?
            };

            self.input.advance(consumed);

            // SAFETY: the engine reported writing `produced` bytes to the front of the slice
            // returned by `first_unfilled_slice`, so exactly that many bytes are initialized.
            unsafe { self.output.advance(produced) };

            self.total_in = self.total_in.saturating_add(u64::try_from(consumed).unwrap_or(u64::MAX));
            self.total_out = self.total_out.saturating_add(u64::try_from(produced).unwrap_or(u64::MAX));
            codec.check_limits(self.total_in, self.total_out)?;

            if step == Step::StreamEnd && codec.stream_ended(!self.input.is_empty()) {
                self.state = State::Done;
                return Ok(self.take_output().map_or(Output::Done, Output::Data));
            }

            if consumed == 0 && produced == 0 {
                if end_of_input {
                    // There is room to write and no more input is coming, yet the engine cannot
                    // finish: the input ended part-way through a container.
                    return Err(Error::unexpected_end_of_stream());
                }

                return Ok(self.take_output().map_or(Output::NeedInput, Output::Data));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;

    /// A codec that copies input to output verbatim, so pump behaviour can be tested on its own.
    #[derive(Debug, Default)]
    struct Passthrough {
        ended: bool,
    }

    impl Codec for Passthrough {
        fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], last_input: bool) -> Result<(Step, usize, usize)> {
            let count = input.len().min(output.len());
            for (slot, byte) in output.iter_mut().zip(input.iter().take(count)) {
                slot.write(*byte);
            }

            if last_input && count == input.len() {
                self.ended = true;
                return Ok((Step::StreamEnd, count, count));
            }

            Ok((Step::Continue, count, count))
        }

        fn stream_ended(&mut self, _more_input_available: bool) -> bool {
            true
        }
    }

    fn chunk(size: usize) -> NonZeroUsize {
        NonZeroUsize::new(size).expect("test chunk sizes are never zero")
    }

    fn view(bytes: &[u8]) -> BytesView {
        BytesView::copied_from_slice(bytes, &GlobalPool::new())
    }

    #[test]
    fn reports_need_input_when_empty() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        let output = pump.pull(&mut Passthrough::default()).expect("pull succeeds");

        assert!(output.is_need_input());
    }

    #[test]
    fn round_trips_data_through_the_codec() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.push(view(b"hello world")).expect("push succeeds");

        let data = pump
            .pull(&mut Passthrough::default())
            .expect("pull succeeds")
            .into_data()
            .expect("data is available");

        assert_eq!(data.to_vec(), b"hello world".to_vec());
        assert_eq!(pump.total_in(), 11);
        assert_eq!(pump.total_out(), 11);
    }

    #[test]
    fn bounds_each_chunk_to_the_configured_size() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(4));
        pump.push(view(b"abcdefghij")).expect("push succeeds");

        let data = pump
            .pull(&mut Passthrough::default())
            .expect("pull succeeds")
            .into_data()
            .expect("data is available");

        assert!(data.len() <= 8, "chunk was {} bytes, expected it near 4", data.len());
    }

    #[test]
    fn rejects_a_second_push_while_input_is_pending() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.push(view(b"first")).expect("push succeeds");

        let error = pump.push(view(b"second")).expect_err("overlapping push is rejected");
        assert!(error.is_invalid_state());
    }

    #[test]
    fn rejects_push_after_finish() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.finish();

        let error = pump.push(view(b"late")).expect_err("push after finish is rejected");
        assert!(error.is_invalid_state());
    }

    #[test]
    fn finish_is_idempotent() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.finish();
        pump.finish();

        let output = pump.pull(&mut Passthrough::default()).expect("pull succeeds");
        assert!(output.is_done());
    }

    #[test]
    fn reports_done_after_the_stream_ends() {
        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.push(view(b"tail")).expect("push succeeds");
        pump.finish();

        let mut codec = Passthrough::default();
        let data = pump.pull(&mut codec).expect("pull succeeds").into_data().expect("data");
        assert_eq!(data.to_vec(), b"tail".to_vec());

        assert!(pump.pull(&mut codec).expect("pull succeeds").is_done());
        assert!(pump.pull(&mut codec).expect("pull succeeds").is_done());
    }

    #[test]
    fn reports_truncation_when_the_codec_never_ends() {
        /// Consumes input but never reports `StreamEnd`, imitating a truncated container.
        #[derive(Debug)]
        struct NeverEnds;

        impl Codec for NeverEnds {
            fn step(&mut self, _input: &[u8], _output: &mut [MaybeUninit<u8>], _last_input: bool) -> Result<(Step, usize, usize)> {
                Ok((Step::Continue, 0, 0))
            }

            fn stream_ended(&mut self, _more_input_available: bool) -> bool {
                true
            }
        }

        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.finish();

        let error = pump.pull(&mut NeverEnds).expect_err("truncation is reported");
        assert!(error.is_unexpected_end_of_stream());
    }

    #[test]
    fn propagates_limit_failures() {
        /// Produces output without consuming input, and rejects it via `check_limits`.
        #[derive(Debug)]
        struct Expanding;

        impl Codec for Expanding {
            fn step(&mut self, _input: &[u8], output: &mut [MaybeUninit<u8>], _last_input: bool) -> Result<(Step, usize, usize)> {
                for slot in output.iter_mut() {
                    slot.write(0);
                }

                Ok((Step::Continue, 0, output.len()))
            }

            fn stream_ended(&mut self, _more_input_available: bool) -> bool {
                true
            }

            fn check_limits(&self, _total_in: u64, total_out: u64) -> Result<()> {
                if total_out > 0 {
                    return Err(Error::limit_exceeded("test limit"));
                }

                Ok(())
            }
        }

        let mut pump = Pump::new(GlobalPool::new(), chunk(64));
        pump.push(view(b"seed")).expect("push succeeds");

        let error = pump.pull(&mut Expanding).expect_err("limit is enforced");
        assert!(error.is_limit_exceeded());
    }
}
