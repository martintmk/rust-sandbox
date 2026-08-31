// Licensed under the MIT License.

//! Zlib (RFC 1950): a deflate payload with a two byte header and an Adler-32 trailer.
//!
//! This is the format behind HTTP `Content-Encoding: deflate`, which despite its name carries a
//! zlib stream rather than raw deflate.
//!
//! # Examples
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::zlib;
//!
//! let memory = GlobalPool::new();
//! let encoded = zlib::compress(
//!     BytesView::copied_from_slice(b"the quick brown fox", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     zlib::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

use crate::flate::Wrapper;
use crate::flate::codec::{FlateCompress, FlateDecompress};
use crate::format::macros::define_format;

define_format! {
    name = "zlib",
    encoder_codec = FlateCompress,
    encoder_options = (),
    new_encoder = |level, (), pool| FlateCompress::new(Wrapper::Zlib, level, pool),
    decoder_codec = FlateDecompress,
    decoder_options = (),
    default_limits = crate::flate::DEFAULT_LIMITS,
    new_decoder = |limits, multi_stream, (), pool| FlateDecompress::new(Wrapper::Zlib, limits, multi_stream, pool),
    multi_stream_default = false,
    multi_stream_doc = "Sets whether concatenated zlib streams decode as one logical stream.\n\nDisabled by default: unlike gzip, concatenating zlib streams is not an established convention.",
}
