// Licensed under the MIT License.

/// A compression level, from [`Level::NONE`] (store only) to [`Level::BEST`].
///
/// This is a newtype rather than a re-export of the underlying compression engine's level type.
/// Exposing the engine's type would make the engine part of this crate's semver surface, so
/// swapping or upgrading it would become a breaking change for every consumer.
///
/// The scale is portable but its *cost* is not, and the difference between formats is large. On
/// the deflate family and on zstd, moving up the scale changes the time taken but barely moves the
/// memory used. On brotli both climb steeply towards the top of the range, while the ratio gained
/// over the middle of the range stays small. Treat [`Level::BEST`] as a deliberate choice to be
/// measured on real payloads, not as a free improvement.
///
/// # Examples
///
/// ```
/// use compressed::Level;
///
/// assert_eq!(Level::default(), Level::DEFAULT);
/// assert_eq!(Level::new(9), Some(Level::BEST));
/// assert_eq!(Level::new(10), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level(u8);

impl Level {
    /// No compression: the data is stored, wrapped in the container format.
    pub const NONE: Self = Self(0);

    /// The fastest level that still compresses.
    pub const FAST: Self = Self(1);

    /// A balanced trade-off between speed and compression ratio.
    pub const DEFAULT: Self = Self(6);

    /// The best compression ratio, at the highest cost in time and, on some formats, in memory.
    ///
    /// See the note on [`Level`] before reaching for this.
    pub const BEST: Self = Self(9);

    /// The weakest level, the same as [`Level::NONE`].
    pub const MIN: Self = Self::NONE;

    /// The strongest level, the same as [`Level::BEST`].
    ///
    /// This is a `Level` rather than a bare number, so it can be passed straight to a builder.
    pub const MAX: Self = Self::BEST;

    /// Creates a level, or returns `None` if `level` exceeds [`Level::MAX`].
    ///
    /// This returns an `Option` rather than panicking because levels routinely arrive from
    /// configuration files and command-line arguments, where an out-of-range value is a user
    /// mistake to be reported rather than a bug to crash on. Use [`TryFrom`] when you want that
    /// mistake as an [`Error`][crate::Error] to propagate with `?`.
    #[must_use]
    pub const fn new(level: u8) -> Option<Self> {
        if level > Self::MAX.0 { None } else { Some(Self(level)) }
    }

    /// Returns the level as a number in `0..=9`.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for Level {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u8> for Level {
    type Error = crate::Error;

    fn try_from(level: u8) -> Result<Self, Self::Error> {
        Self::new(level).ok_or_else(|| {
            crate::Error::invalid_configuration(format!(
                "compression level {level} is out of range; expected {}..={}",
                Self::MIN.0,
                Self::MAX.0
            ))
        })
    }
}

impl From<Level> for u8 {
    fn from(level: Level) -> Self {
        level.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_the_whole_valid_range() {
        for level in 0..=Level::MAX.get() {
            let parsed = Level::new(level).expect("level is within range");
            assert_eq!(parsed.get(), level);
        }
    }

    #[test]
    fn new_rejects_out_of_range_without_panicking() {
        assert_eq!(Level::new(10), None);
        assert_eq!(Level::new(u8::MAX), None);
    }

    #[test]
    fn bounds_are_levels_so_they_can_be_passed_to_a_builder() {
        assert_eq!(Level::MIN, Level::NONE);
        assert_eq!(Level::MAX, Level::BEST);
    }

    #[test]
    fn conversions_follow_the_standard_traits() {
        assert_eq!(Level::try_from(9).expect("in range"), Level::BEST);
        assert_eq!(u8::from(Level::BEST), 9);

        let error = Level::try_from(10).expect_err("out of range");
        assert!(error.is_invalid_configuration(), "got {error}");
        assert!(error.to_string().contains("0..=9"), "the message should name the range: {error}");
    }

    #[test]
    fn named_levels_have_the_expected_values() {
        assert_eq!(Level::NONE.get(), 0);
        assert_eq!(Level::FAST.get(), 1);
        assert_eq!(Level::DEFAULT.get(), 6);
        assert_eq!(Level::BEST.get(), 9);
    }

    #[test]
    fn default_matches_the_default_constant() {
        assert_eq!(Level::default(), Level::DEFAULT);
    }

    #[test]
    fn levels_order_by_strength() {
        assert!(Level::NONE < Level::FAST);
        assert!(Level::FAST < Level::DEFAULT);
        assert!(Level::DEFAULT < Level::BEST);
    }
}
