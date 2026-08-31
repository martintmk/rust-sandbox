// Licensed under the MIT License.

//! Raw deflate (RFC 1951): the compressed payload with no header and no checksum.
//!
//! Use this only where the surrounding format supplies its own framing and integrity check, such
//! as inside a ZIP archive or a PNG chunk. Without a checksum, corruption is not reliably detected,
//! so prefer [`zlib`][crate::zlib] or [`gzip`][crate::gzip] for data in transit.
//!
//! # Examples
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::deflate;
//!
//! let memory = GlobalPool::new();
//! let encoded = deflate::compress(
//!     BytesView::copied_from_slice(b"the quick brown fox", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     deflate::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

use crate::flate::Wrapper;
use crate::flate::codec::{FlateCompress, FlateDecompress};
use crate::format::macros::define_format;

define_format! {
    name = "deflate",
    encoder_codec = FlateCompress,
    encoder_options = (),
    new_encoder = |level, (), pool| FlateCompress::new(Wrapper::Raw, level, pool),
    decoder_codec = FlateDecompress,
    decoder_options = (),
    default_limits = crate::flate::DEFAULT_LIMITS,
    new_decoder = |limits, multi_stream, (), pool| FlateDecompress::new(Wrapper::Raw, limits, multi_stream, pool),
    multi_stream_default = false,
    multi_stream_doc = "Sets whether consecutive deflate streams decode as one logical stream.\n\nDisabled by default: raw deflate carries no framing, so trailing bytes are usually not another stream.",
}
