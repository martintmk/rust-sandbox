// Licensed under the MIT License.

//! The deflate family: raw deflate, zlib and gzip.
//!
//! All three wrap the same deflate payload, differing only in framing, so they share one codec
//! implementation parameterised by [`Wrapper`].

use std::mem::MaybeUninit;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::engine::{Codec, Step};
use crate::error::{Error, Result};
use crate::level::Level;
use crate::limits::DecompressionLimits;

/// The deflate window size exponent. 15 is the maximum, giving the best compression ratio.
const WINDOW_BITS: u8 = 15;

/// The container framing wrapped around a deflate payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wrapper {
    /// Raw deflate (RFC 1951): no header and no checksum.
    Raw,
    /// zlib (RFC 1950): a two byte header and an Adler-32 trailer.
    Zlib,
    /// gzip (RFC 1952): a ten byte header and a CRC-32 plus length trailer.
    Gzip,
}

impl Wrapper {
    fn compressor(self, level: Level) -> Compress {
        let compression = Compression::new(u32::from(level.get()));

        match self {
            Self::Raw => Compress::new(compression, false),
            Self::Zlib => Compress::new(compression, true),
            Self::Gzip => Compress::new_gzip(compression, WINDOW_BITS),
        }
    }

    fn decompressor(self) -> Decompress {
        match self {
            Self::Raw => Decompress::new(false),
            Self::Zlib => Decompress::new(true),
            Self::Gzip => Decompress::new_gzip(WINDOW_BITS),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Raw => "deflate",
            Self::Zlib => "zlib",
            Self::Gzip => "gzip",
        }
    }
}

#[derive(Debug)]
pub(crate) struct FlateCompress {
    compress: Compress,
}

impl FlateCompress {
    pub(crate) fn new(wrapper: Wrapper, level: Level) -> Self {
        Self {
            compress: wrapper.compressor(level),
        }
    }
}

impl Codec for FlateCompress {
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

#[derive(Debug)]
pub(crate) struct FlateDecompress {
    decompress: Decompress,
    wrapper: Wrapper,
    limits: DecompressionLimits,
    concatenated: bool,
}

impl FlateDecompress {
    pub(crate) fn new(wrapper: Wrapper, limits: DecompressionLimits, concatenated: bool) -> Self {
        Self {
            decompress: wrapper.decompressor(),
            wrapper,
            limits,
            concatenated,
        }
    }
}

impl Codec for FlateDecompress {
    fn step(&mut self, input: &[u8], output: &mut [MaybeUninit<u8>], _last_input: bool) -> Result<(Step, usize, usize)> {
        let before_in = self.decompress.total_in();
        let before_out = self.decompress.total_out();

        let status = self
            .decompress
            .decompress_uninit(input, output, FlushDecompress::None)
            .map_err(|error| {
                Error::corrupt_data(format!("the compressed data is not a valid {} stream", self.wrapper.name())).with_source(error)
            })?;

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
        if !self.concatenated || !more_input_available {
            return true;
        }

        // Another stream follows. The engine must be replaced rather than reset: `Decompress::reset`
        // takes a `bool` that selects between raw deflate and zlib, and so cannot express gzip
        // framing (which the engine encodes as `window_bits + 16`). Resetting a gzip decoder
        // silently drops it to raw deflate, and the next member then fails with "invalid block
        // type".
        self.decompress = self.wrapper.decompressor();
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
    fn every_wrapper_has_a_name() {
        assert_eq!(Wrapper::Raw.name(), "deflate");
        assert_eq!(Wrapper::Zlib.name(), "zlib");
        assert_eq!(Wrapper::Gzip.name(), "gzip");
    }

    #[test]
    fn wrappers_produce_distinguishable_headers() {
        // Guards the framing: a zlib stream must not be mistaken for a gzip one, and raw deflate
        // must carry no header at all.
        let mut headers = Vec::new();

        for wrapper in [Wrapper::Raw, Wrapper::Zlib, Wrapper::Gzip] {
            let mut codec = FlateCompress::new(wrapper, Level::DEFAULT);
            let mut out = [MaybeUninit::uninit(); 64];
            let (_, _, produced) = codec.step(b"header check", &mut out, true).expect("compression succeeds");

            // SAFETY: the engine reported initializing `produced` bytes.
            let bytes = unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), produced) };
            headers.push(bytes[..2].to_vec());
        }

        assert_eq!(headers[2], vec![0x1f, 0x8b], "gzip must carry its magic bytes");
        assert_ne!(headers[0], headers[1], "raw deflate and zlib must differ");
        assert_ne!(headers[1], headers[2], "zlib and gzip must differ");
    }
}
