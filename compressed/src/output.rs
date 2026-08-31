// Licensed under the MIT License.

use bytesbuf::BytesView;

/// What a single codec step produced.
///
/// This is the state machine a caller drives: keep calling `pull` until it reports
/// [`Output::NeedInput`], supply more data, and stop at [`Output::Done`].
///
/// It is an enum rather than an `Option<BytesView>` plus a separate `is_finished()` because
/// "no bytes right now" and "no bytes ever again" require different responses from the caller, and
/// conflating them turns a missing check into an infinite loop.
///
/// It is deliberately *not* `#[non_exhaustive]`. These three states describe a complete codec step,
/// and a caller that fails to handle one has a bug. Forcing a wildcard arm would convert that bug
/// from a compile error into silent misbehaviour, which is the opposite of what a wildcard is for.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "a BytesView is ~272 bytes because it stores its first spans inline; boxing it would               add an allocation per chunk on the hot path, which is exactly what this crate exists               to avoid"
)]
pub enum Output {
    /// Bytes are available now.
    ///
    /// Never empty.
    Data(BytesView),

    /// More input is required before more output can be produced.
    NeedInput,

    /// The stream ended cleanly and no further output will ever be produced.
    Done,
}

impl Output {
    /// Returns the bytes, if this is [`Output::Data`].
    #[must_use]
    pub fn into_data(self) -> Option<BytesView> {
        match self {
            Self::Data(data) => Some(data),
            _ => None,
        }
    }

    /// Whether the codec needs more input before it can produce more output.
    #[must_use]
    pub fn is_need_input(&self) -> bool {
        matches!(*self, Self::NeedInput)
    }

    /// Whether the stream has ended.
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(*self, Self::Done)
    }
}

#[cfg(test)]
mod tests {
    use bytesbuf::mem::GlobalPool;

    use super::*;

    #[test]
    fn into_data_returns_bytes_only_for_data() {
        let memory = GlobalPool::new();
        let view = BytesView::copied_from_slice(b"hello", &memory);

        assert_eq!(Output::Data(view).into_data().map(|d| d.to_vec()), Some(b"hello".to_vec()));
        assert!(Output::NeedInput.into_data().is_none());
        assert!(Output::Done.into_data().is_none());
    }

    #[test]
    fn predicates_identify_exactly_one_variant() {
        let memory = GlobalPool::new();
        let data = Output::Data(BytesView::copied_from_slice(b"x", &memory));

        assert!(!data.is_need_input() && !data.is_done());
        assert!(Output::NeedInput.is_need_input() && !Output::NeedInput.is_done());
        assert!(Output::Done.is_done() && !Output::Done.is_need_input());
    }

    #[test]
    fn debug_is_available_for_diagnostics() {
        assert!(format!("{:?}", Output::NeedInput).contains("NeedInput"));
    }
}
