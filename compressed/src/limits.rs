// Licensed under the MIT License.

use std::num::NonZeroU32;

use crate::error::{Error, Result};

/// Cumulative output below this size is never rejected by the ratio guard.
///
/// A gzip member carries 18 bytes of fixed header and trailer overhead, and a short member's
/// compressed form can easily be larger than its payload. Without a floor, a legitimate two-byte
/// member would look like an infinitely bad expansion ratio and be rejected. 32 KiB is far below
/// any size at which a decompression bomb becomes a memory-exhaustion risk.
#[cfg_attr(
    all(not(test), not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib"))),
    expect(dead_code, reason = "only the decoders enforce limits, and no format is enabled")
)]
const RATIO_FLOOR_BYTES: u64 = 32 * 1024;

/// The ratio limit that suits the deflate family.
///
/// Deflate cannot expand its input by more than about 1032x — that is a structural property of the
/// format, not a tuning choice — so a single deflate, zlib or gzip stream is inherently bounded.
/// Measured worst case for 1 MiB of zeros is 1015x, so this sits just above what the format can
/// actually produce.
const DEFLATE_MAX_RATIO: u32 = 1_100;

/// The ratio limit that suits brotli.
///
/// Brotli has no comparable structural ceiling: measured on ordinary repetitive input it reaches
/// 9 000x for a repeated short string, 10 900x for repetitive JSON, 21 000x for a repeated
/// sentence, and 80 660x for 1 MiB of zeros — all legitimate data. A deflate-shaped limit rejects
/// every one of them, so brotli needs its own, and even this one is a coarse backstop rather than
/// real protection.
const BROTLI_MAX_RATIO: u32 = 250_000;

/// Bounds on how much data decompression may produce.
///
/// Compressed formats can expand their input by many orders of magnitude, so a decoder pointed at
/// untrusted data is a memory-exhaustion vector unless it is bounded. The default is a *ratio*
/// limit rather than an absolute byte cap, because an absolute cap would reject exactly the large
/// legitimate streams that streaming decompression exists to serve.
///
/// ```
/// use std::num::NonZeroU32;
///
/// use compressed::DecompressionLimits;
///
/// // Tighter than the default, for input from an untrusted peer.
/// let limits = DecompressionLimits::DEFAULT
///     .with_max_ratio(NonZeroU32::new(50).unwrap())
///     .with_max_output_len(16 * 1024 * 1024);
/// # let _ = limits;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecompressionLimits {
    max_ratio: Option<u32>,
    max_output_len: Option<u64>,
}

impl DecompressionLimits {
    /// The limits for the deflate family: expansion capped at 1100x, no cap on total size.
    ///
    /// This is what [`Default`] returns and what the [`deflate`][crate::deflate],
    /// [`zlib`][crate::zlib] and [`gzip`][crate::gzip] decoders use unless told otherwise. It sits
    /// just above deflate's structural ceiling of roughly 1032x, so it never rejects data the
    /// format could legitimately have produced.
    pub const DEFAULT: Self = Self {
        max_ratio: Some(DEFLATE_MAX_RATIO),
        max_output_len: None,
    };

    /// The limits for brotli: expansion capped at 250 000x, no cap on total size.
    ///
    /// Brotli legitimately reaches tens of thousands of times expansion on repetitive input, so it
    /// needs a far looser ratio than the deflate family. That makes the ratio a coarse backstop
    /// rather than real protection.
    ///
    /// # Security
    ///
    /// For untrusted brotli input, set [`with_max_output_len`][Self::with_max_output_len] to
    /// whatever your caller can actually afford to buffer. A ratio limit cannot separate a bomb
    /// from legitimate highly-compressible data in a format with no expansion ceiling.
    pub const BROTLI: Self = Self {
        max_ratio: Some(BROTLI_MAX_RATIO),
        max_output_len: None,
    };

    /// Applies no limits at all.
    ///
    /// # Security
    ///
    /// Only use this when the compressed data comes from a source you trust to the same degree you
    /// trust your own process. An unbounded decoder fed a decompression bomb will consume memory
    /// until the allocator gives up.
    pub const UNLIMITED: Self = Self {
        max_ratio: None,
        max_output_len: None,
    };

    /// Sets the largest permitted ratio of decompressed to compressed bytes.
    ///
    /// The ratio is only enforced once cumulative output exceeds 32 KiB, so small streams are never
    /// rejected for the fixed overhead of their container format.
    #[must_use]
    pub const fn with_max_ratio(mut self, ratio: NonZeroU32) -> Self {
        self.max_ratio = Some(ratio.get());
        self
    }

    /// Sets the largest permitted total decompressed size, in bytes.
    #[must_use]
    pub const fn with_max_output_len(mut self, bytes: u64) -> Self {
        self.max_output_len = Some(bytes);
        self
    }

    /// Fails if the totals so far violate either limit.
    #[cfg_attr(
        all(not(test), not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib"))),
        expect(dead_code, reason = "only the decoders enforce limits, and no format is enabled")
    )]
    pub(crate) fn check(self, input_len: u64, output_len: u64) -> Result<()> {
        if let Some(max) = self.max_output_len
            && output_len > max
        {
            return Err(Error::limit_exceeded(format!(
                "decompressed output reached {output_len} bytes, exceeding the limit of {max}"
            )));
        }

        if let Some(ratio) = self.max_ratio
            && output_len > RATIO_FLOOR_BYTES
            && output_len > input_len.saturating_mul(u64::from(ratio))
        {
            return Err(Error::limit_exceeded(format!(
                "decompressed output reached {output_len} bytes from {input_len} compressed bytes, \
                 exceeding the expansion limit of {ratio}x"
            )));
        }

        Ok(())
    }
}

impl Default for DecompressionLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ratio(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("test ratios are never zero")
    }

    #[test]
    fn default_matches_the_default_constant() {
        assert_eq!(DecompressionLimits::default(), DecompressionLimits::DEFAULT);
    }

    #[test]
    fn unlimited_accepts_an_absurd_expansion() {
        DecompressionLimits::UNLIMITED.check(1, u64::MAX).expect("unlimited never rejects");
    }

    #[test]
    fn ratio_guard_rejects_a_bomb() {
        let error = DecompressionLimits::DEFAULT
            .check(1_000, 100 * 1024 * 1024)
            .expect_err("100 MB from 1 KB is a bomb");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn ratio_guard_allows_ordinary_data() {
        // Measured ratio for ordinary text is roughly 20x, well inside the 1000x default.
        DecompressionLimits::DEFAULT
            .check(512 * 1024, 10 * 1024 * 1024)
            .expect("20x expansion is ordinary");
    }

    #[test]
    fn ratio_guard_allows_multi_gigabyte_streams() {
        // An absolute cap would reject this; a ratio guard must not.
        DecompressionLimits::DEFAULT
            .check(64 * 1024 * 1024 * 1024, 640 * 1024 * 1024 * 1024)
            .expect("a 640 GB stream at 10x expansion is legitimate");
    }

    #[test]
    fn ratio_guard_ignores_output_below_the_floor() {
        // Worst case: more output than the floor allows in ratio terms, but under the floor.
        DecompressionLimits::DEFAULT
            .check(0, RATIO_FLOOR_BYTES)
            .expect("small outputs are never rejected on ratio");
    }

    #[test]
    fn ratio_guard_engages_immediately_above_the_floor() {
        let error = DecompressionLimits::DEFAULT
            .check(0, RATIO_FLOOR_BYTES + 1)
            .expect_err("zero input can never justify output above the floor");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn absolute_limit_rejects_beyond_the_cap() {
        let error = DecompressionLimits::UNLIMITED
            .with_max_output_len(100)
            .check(1_000_000, 101)
            .expect_err("101 bytes exceeds a 100 byte cap");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn absolute_limit_allows_exactly_the_cap() {
        DecompressionLimits::UNLIMITED
            .with_max_output_len(100)
            .check(1_000_000, 100)
            .expect("the cap itself is allowed");
    }

    #[test]
    fn custom_ratio_is_applied() {
        let limits = DecompressionLimits::UNLIMITED.with_max_ratio(ratio(2));

        limits
            .check(RATIO_FLOOR_BYTES, RATIO_FLOOR_BYTES * 2)
            .expect("exactly 2x is allowed");

        let error = limits
            .check(RATIO_FLOOR_BYTES, RATIO_FLOOR_BYTES * 2 + 1)
            .expect_err("just past 2x is rejected");
        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn ratio_multiplication_saturates_instead_of_overflowing() {
        let limits = DecompressionLimits::UNLIMITED.with_max_ratio(ratio(u32::MAX));

        limits
            .check(u64::MAX, u64::MAX)
            .expect("saturating multiplication must not panic or wrap");
    }
}
