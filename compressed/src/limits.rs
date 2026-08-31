// Licensed under the MIT License.

use std::num::NonZeroU32;

use crate::error::{Error, Result};

/// Cumulative output below this size is never rejected by the ratio guard.
///
/// A container carries a fixed header and trailer, and a short stream's compressed form can easily
/// be larger than its payload. Without a floor, a legitimate two-byte stream would look like an
/// infinitely bad expansion ratio and be rejected. 32 KiB is far below any size at which a
/// decompression bomb becomes a memory-exhaustion risk.
#[cfg_attr(
    all(
        not(test),
        not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))
    ),
    expect(dead_code, reason = "only the decoders resolve and enforce bounds, and no format is enabled")
)]
const RATIO_FLOOR_BYTES: u64 = 32 * 1024;

/// One configurable bound, in one of three states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Limit<T> {
    /// The caller expressed no opinion, so the format's own default applies.
    #[default]
    Unset,
    /// The caller explicitly removed the bound.
    Unlimited,
    /// The caller explicitly chose a bound.
    Value(T),
}

impl<T> Limit<T> {
    #[cfg_attr(
        all(
            not(test),
            not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))
        ),
        expect(dead_code, reason = "only the decoders resolve and enforce bounds, and no format is enabled")
    )]
    fn resolve(self, default: Option<T>) -> Option<T> {
        match self {
            Self::Unset => default,
            Self::Unlimited => None,
            Self::Value(value) => Some(value),
        }
    }
}

/// Bounds on how much data decompression may produce.
///
/// Compressed data can expand by orders of magnitude, so a decoder pointed at untrusted input is a
/// memory-exhaustion vector.
///
/// This type carries *overrides*, not values. Each bound starts unset, meaning the format applies
/// its own default — there is no portable default, because the formats differ by orders of
/// magnitude in what they can legitimately produce:
///
/// | Format | Default ratio bound | Why |
/// |---|---|---|
/// | `deflate`, `zlib`, `gzip` | 1100x | deflate cannot expand further than about 1032x; that is structural |
/// | `brotli` | 250 000x | brotli reaches 80 660x on a megabyte of zeros, and 21 028x on a repeated sentence — all legitimate |
/// | `zstd` | 250 000x | zstd has no structural ceiling either, so it needs the same loose bound |
///
/// No format caps total output size by default, so a multi-gigabyte stream decodes.
///
/// # Security
///
/// A ratio bound is a coarse backstop, not real protection: in a format with no structural
/// expansion ceiling it cannot separate a bomb from legitimate highly-compressible data. For
/// untrusted input set [`with_max_output_len`][Self::with_max_output_len] to whatever the caller
/// can actually afford to buffer.
///
/// # Examples
///
/// ```
/// use std::num::NonZeroU32;
///
/// use compressed::DecompressionLimits;
///
/// // Leave the format's own ratio default alone, but cap what we will buffer.
/// let untrusted = DecompressionLimits::new().with_max_output_len(16 * 1024 * 1024);
///
/// // Or override both.
/// let strict = DecompressionLimits::new()
///     .with_max_ratio(NonZeroU32::new(50).unwrap())
///     .with_max_output_len(1024 * 1024);
/// # let _ = (untrusted, strict);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecompressionLimits {
    max_ratio: Limit<u32>,
    max_output_len: Limit<u64>,
}

impl DecompressionLimits {
    /// Overrides nothing: every bound is left to the format's own default.
    ///
    /// This is what [`Default`] returns.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_ratio: Limit::Unset,
            max_output_len: Limit::Unset,
        }
    }

    /// Removes every bound, overriding whatever the format would have applied.
    ///
    /// # Security
    ///
    /// Only use this when the compressed data comes from a source you trust to the same degree you
    /// trust your own process. An unbounded decoder fed a decompression bomb will consume memory
    /// until the allocator gives up.
    pub const UNLIMITED: Self = Self {
        max_ratio: Limit::Unlimited,
        max_output_len: Limit::Unlimited,
    };

    /// Bounds the ratio of decompressed to compressed bytes.
    ///
    /// The ratio is only enforced once cumulative output exceeds 32 KiB, so small streams are never
    /// rejected for the fixed overhead of their container.
    #[must_use]
    pub const fn with_max_ratio(mut self, ratio: NonZeroU32) -> Self {
        self.max_ratio = Limit::Value(ratio.get());
        self
    }

    /// Removes the ratio bound, overriding the format's default.
    #[must_use]
    pub const fn without_max_ratio(mut self) -> Self {
        self.max_ratio = Limit::Unlimited;
        self
    }

    /// Bounds the total decompressed size, in bytes.
    ///
    /// This is the bound that actually protects a caller which buffers the output.
    #[must_use]
    pub const fn with_max_output_len(mut self, bytes: u64) -> Self {
        self.max_output_len = Limit::Value(bytes);
        self
    }

    /// Removes the total size bound, overriding the format's default.
    #[must_use]
    pub const fn without_max_output_len(mut self) -> Self {
        self.max_output_len = Limit::Unlimited;
        self
    }

    /// Applies these overrides on top of a format's defaults.
    #[cfg_attr(
        all(
            not(test),
            not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))
        ),
        expect(dead_code, reason = "only the decoders resolve and enforce bounds, and no format is enabled")
    )]
    pub(crate) fn resolve(self, defaults: FormatLimits) -> FormatLimits {
        FormatLimits {
            max_ratio: self.max_ratio.resolve(defaults.max_ratio),
            max_output_len: self.max_output_len.resolve(defaults.max_output_len),
        }
    }
}

/// A format's bounds after the caller's overrides have been applied.
///
/// Private: formats declare their defaults as constants of this type, and the decoders enforce it.
#[cfg_attr(
    all(
        not(test),
        not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))
    ),
    expect(dead_code, reason = "only the decoders resolve and enforce bounds, and no format is enabled")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormatLimits {
    max_ratio: Option<u32>,
    max_output_len: Option<u64>,
}

#[cfg_attr(
    all(
        not(test),
        not(any(feature = "brotli", feature = "deflate", feature = "gzip", feature = "zlib", feature = "zstd"))
    ),
    expect(dead_code, reason = "only the decoders resolve and enforce bounds, and no format is enabled")
)]
impl FormatLimits {
    /// Declares a format's default bounds.
    pub(crate) const fn new(max_ratio: Option<u32>, max_output_len: Option<u64>) -> Self {
        Self { max_ratio, max_output_len }
    }

    /// Fails if the totals so far violate either bound.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a format's declared defaults.
    const DEFAULTS: FormatLimits = FormatLimits::new(Some(1_000), None);

    fn ratio(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("test ratios are never zero")
    }

    fn resolved(limits: DecompressionLimits) -> FormatLimits {
        limits.resolve(DEFAULTS)
    }

    #[test]
    fn default_overrides_nothing() {
        assert_eq!(DecompressionLimits::default(), DecompressionLimits::new());
        assert_eq!(resolved(DecompressionLimits::default()), DEFAULTS);
    }

    #[test]
    fn an_unset_bound_defers_to_the_format() {
        // The whole point of the override model: a caller who cares about one bound must not
        // silently clobber the other with a value calibrated for a different format.
        let limits = DecompressionLimits::new().with_max_output_len(4096);
        let resolved = resolved(limits);

        assert_eq!(resolved.max_ratio, DEFAULTS.max_ratio, "the format's ratio must survive");
        assert_eq!(resolved.max_output_len, Some(4096));
    }

    #[test]
    fn unlimited_removes_the_formats_defaults() {
        let resolved = resolved(DecompressionLimits::UNLIMITED);

        assert_eq!(resolved.max_ratio, None);
        assert_eq!(resolved.max_output_len, None);
        resolved.check(1, u64::MAX).expect("unlimited never rejects");
    }

    #[test]
    fn each_bound_can_be_removed_independently() {
        let no_ratio = resolved(DecompressionLimits::new().without_max_ratio());
        assert_eq!(no_ratio.max_ratio, None);
        assert_eq!(no_ratio.max_output_len, DEFAULTS.max_output_len);

        let no_len = resolved(DecompressionLimits::new().without_max_output_len());
        assert_eq!(no_len.max_ratio, DEFAULTS.max_ratio);
        assert_eq!(no_len.max_output_len, None);
    }

    #[test]
    fn an_explicit_bound_overrides_the_format() {
        let resolved = resolved(DecompressionLimits::new().with_max_ratio(ratio(7)));

        assert_eq!(resolved.max_ratio, Some(7));
    }

    #[test]
    fn ratio_guard_rejects_a_bomb() {
        let error = DEFAULTS.check(1_000, 100 * 1024 * 1024).expect_err("100 MB from 1 KB is a bomb");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn ratio_guard_allows_multi_gigabyte_streams() {
        // An absolute cap would reject this; a ratio guard must not.
        DEFAULTS
            .check(64 * 1024 * 1024 * 1024, 640 * 1024 * 1024 * 1024)
            .expect("a 640 GB stream at 10x expansion is legitimate");
    }

    #[test]
    fn ratio_guard_ignores_output_below_the_floor() {
        DEFAULTS
            .check(0, RATIO_FLOOR_BYTES)
            .expect("small outputs are never rejected on ratio");
    }

    #[test]
    fn ratio_guard_engages_immediately_above_the_floor() {
        let error = DEFAULTS
            .check(0, RATIO_FLOOR_BYTES + 1)
            .expect_err("zero input can never justify output above the floor");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn absolute_bound_rejects_beyond_the_cap() {
        let limits = resolved(DecompressionLimits::new().with_max_output_len(100));
        let error = limits.check(1_000_000, 101).expect_err("101 bytes exceeds a 100 byte cap");

        assert!(error.is_limit_exceeded());
    }

    #[test]
    fn absolute_bound_allows_exactly_the_cap() {
        let limits = resolved(DecompressionLimits::new().with_max_output_len(100));

        limits.check(1_000_000, 100).expect("the cap itself is allowed");
    }

    #[test]
    fn ratio_multiplication_saturates_instead_of_overflowing() {
        let limits = resolved(DecompressionLimits::new().with_max_ratio(ratio(u32::MAX)));

        limits
            .check(u64::MAX, u64::MAX)
            .expect("saturating multiplication must not panic or wrap");
    }
}
