// Licensed under the MIT License.

/// A compression level, from [`Level::NONE`] (store only) to [`Level::BEST`].
///
/// This is a newtype rather than a re-export of the underlying compression engine's level type.
/// Exposing the engine's type would make the engine part of this crate's semver surface, so
/// swapping or upgrading it would become a breaking change for every consumer.
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
    /// The highest level this type can represent.
    pub const MAX: u8 = 9;

    /// No compression: the data is stored, wrapped in the container format.
    pub const NONE: Self = Self(0);

    /// The fastest level that still compresses.
    pub const FAST: Self = Self(1);

    /// A balanced trade-off between speed and compression ratio.
    pub const DEFAULT: Self = Self(6);

    /// The best compression ratio, at the highest cost in time.
    pub const BEST: Self = Self(Self::MAX);

    /// Creates a level, or returns `None` if `level` exceeds [`Level::MAX`].
    ///
    /// This returns an `Option` rather than panicking because levels routinely arrive from
    /// configuration files and command-line arguments, where an out-of-range value is a user
    /// mistake to be reported rather than a bug to crash on.
    #[must_use]
    pub const fn new(level: u8) -> Option<Self> {
        if level > Self::MAX { None } else { Some(Self(level)) }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_the_whole_valid_range() {
        for level in 0..=Level::MAX {
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
