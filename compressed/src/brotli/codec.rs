// Licensed under the MIT License.

//! The brotli codec.
//!
//! Brotli is a genuinely different engine from the deflate family: a different state type, a
//! different way of signalling completion, and an output slice that must already be initialized.
//! It is the format that proves the [`Codec`] abstraction is not just shaped around flate2.

use std::mem::MaybeUninit;

use brotli::enc::StandardAlloc;
use brotli::enc::encode::{BrotliEncoderOperation, BrotliEncoderStateStruct};
use brotli::{BrotliDecompressStream, BrotliResult, BrotliState, HeapAlloc, HuffmanCode};

use crate::brotli::{EncoderOptions, Mode};
use crate::engine::{Codec, Step};
use crate::error::{Error, Result};
use crate::level::Level;
use crate::limits::FormatLimits;

/// Brotli's native quality range is `0..=11`, wider than the portable [`Level`] scale of `0..=9`.
///
/// A round-to-nearest linear map, so the endpoints line up (`0 -> 0`, `9 -> 11`) and the mapping
/// stays monotonic.
fn quality(level: Level) -> u32 {
    let scaled = (u32::from(level.get()) * 11 + 4) / 9;
    scaled.min(11)
}

/// Initializes an uninitialized output slice so brotli, which writes into `&mut [u8]`, can use it.
///
/// The deflate backend performs the same zero-fill internally, so this is not extra work relative
/// to the other formats.
fn initialize(output: &mut [MaybeUninit<u8>]) -> &mut [u8] {
    for slot in &mut *output {
        slot.write(0);
    }

    // SAFETY: every element of the slice was just initialized by the loop above, and `u8` has the
    // same layout as `MaybeUninit<u8>`.
    unsafe { &mut *(std::ptr::from_mut(output) as *mut [u8]) }
}

pub(crate) struct BrotliCompress {
    state: BrotliEncoderStateStruct<StandardAlloc>,
    finished: bool,
}

impl BrotliCompress {
    pub(crate) fn new(level: Level, options: EncoderOptions) -> Self {
        use brotli::enc::encode::BrotliEncoderParameter;

        let mut state = BrotliEncoderStateStruct::new(StandardAlloc::default());
        state.set_parameter(BrotliEncoderParameter::BROTLI_PARAM_QUALITY, quality(level));
        state.set_parameter(BrotliEncoderParameter::BROTLI_PARAM_LGWIN, u32::from(options.window_size.get()));
        state.set_parameter(BrotliEncoderParameter::BROTLI_PARAM_MODE, mode(options.mode));

        Self { state, finished: false }
    }
}

/// Maps our [`Mode`] onto brotli's numeric parameter.
fn mode(mode: Mode) -> u32 {
    match mode {
        Mode::Generic => 0,
        Mode::Text => 1,
        Mode::Font => 2,
    }
}

impl std::fmt::Debug for BrotliCompress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrotliCompress")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Codec for BrotliCompress {
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], last_input: bool) -> Result<(Step, usize, usize)> {
        let operation = if last_input {
            BrotliEncoderOperation::BROTLI_OPERATION_FINISH
        } else {
            BrotliEncoderOperation::BROTLI_OPERATION_PROCESS
        };

        let out = initialize(output);
        let mut available_in = input.len();
        let mut input_offset = 0_usize;
        let mut available_out = out.len();
        let mut output_offset = 0_usize;
        let mut total_out = None;

        let ok = self.state.compress_stream(
            operation,
            &mut available_in,
            input,
            &mut input_offset,
            &mut available_out,
            out,
            &mut output_offset,
            &mut total_out,
            &mut |_, _, _, _| (),
        );

        if !ok {
            return Err(Error::invalid_state("the brotli compression engine reported a failure"));
        }

        self.finished = self.state.is_finished();
        let step = if self.finished { Step::StreamEnd } else { Step::Continue };

        Ok((step, input_offset, output_offset))
    }

    fn stream_ended(&mut self, _more_input_available: bool) -> bool {
        true
    }
}

pub(crate) struct BrotliDecompress {
    state: BrotliState<HeapAlloc<u8>, HeapAlloc<u32>, HeapAlloc<HuffmanCode>>,
    limits: FormatLimits,
    multi_stream: bool,
    total_out: usize,
}

impl BrotliDecompress {
    pub(crate) fn new(limits: FormatLimits, multi_stream: bool) -> Self {
        Self {
            state: Self::state(),
            limits,
            multi_stream,
            total_out: 0,
        }
    }

    fn state() -> BrotliState<HeapAlloc<u8>, HeapAlloc<u32>, HeapAlloc<HuffmanCode>> {
        BrotliState::new(HeapAlloc::new(0), HeapAlloc::new(0), HeapAlloc::new(HuffmanCode::default()))
    }
}

impl std::fmt::Debug for BrotliDecompress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrotliDecompress")
            .field("limits", &self.limits)
            .field("multi_stream", &self.multi_stream)
            .finish_non_exhaustive()
    }
}

impl Codec for BrotliDecompress {
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], _last_input: bool) -> Result<(Step, usize, usize)> {
        let out = initialize(output);
        let mut available_in = input.len();
        let mut input_offset = 0_usize;
        let mut available_out = out.len();
        let mut output_offset = 0_usize;

        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            input,
            &mut available_out,
            &mut output_offset,
            out,
            &mut self.total_out,
            &mut self.state,
        );

        let step = match result {
            BrotliResult::ResultSuccess => Step::StreamEnd,
            BrotliResult::NeedsMoreInput | BrotliResult::NeedsMoreOutput => Step::Continue,
            BrotliResult::ResultFailure => {
                return Err(Error::corrupt_data("the compressed data is not a valid brotli stream"));
            }
        };

        Ok((step, input_offset, output_offset))
    }

    fn stream_ended(&mut self, more_input_available: bool) -> bool {
        if !self.multi_stream || !more_input_available {
            return true;
        }

        self.state = Self::state();
        self.total_out = 0;
        false
    }

    fn check_limits(&self, total_in: u64, total_out: u64) -> Result<()> {
        self.limits.check(total_in, total_out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_maps_the_portable_scale_onto_brotlis_range() {
        assert_eq!(quality(Level::NONE), 0, "the floor must line up");
        assert_eq!(quality(Level::BEST), 11, "the ceiling must line up");

        let mut previous = None;
        for raw in 0..=Level::MAX.get() {
            let level = Level::new(raw).expect("level is in range");
            let mapped = quality(level);

            assert!(Some(mapped) > previous, "mapping must be strictly monotonic at level {raw}");
            assert!(mapped <= 11, "level {raw} mapped outside brotli's range");
            previous = Some(mapped);
        }
    }

    #[test]
    fn initialize_zeroes_the_whole_slice() {
        let mut raw = [MaybeUninit::new(0xff_u8); 8];
        let initialized = initialize(&mut raw);

        assert_eq!(initialized, &[0_u8; 8]);
    }
}
