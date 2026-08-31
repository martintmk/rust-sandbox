// Licensed under the MIT License.

//! Brotli (RFC 7932): a general-purpose compressor with a static dictionary tuned for web content.
//!
//! Compresses text noticeably better than [`gzip`][crate::gzip] at comparable speed, which is why
//! it is the usual choice for HTTP `Content-Encoding: br`. Requires the `brotli` cargo feature.
//!
//! Brotli streams carry no magic bytes, so the format has to be known from context, such as a
//! `Content-Encoding` header.
//!
//! ```
//! use bytesbuf::BytesView;
//! use bytesbuf::mem::GlobalPool;
//! use compressed::brotli;
//!
//! let memory = GlobalPool::new();
//! let encoded = brotli::compress(
//!     BytesView::copied_from_slice(b"the quick brown fox", &memory),
//!     memory.clone(),
//! )?;
//!
//! assert_eq!(
//!     brotli::decompress(encoded, memory)?.to_vec(),
//!     b"the quick brown fox".to_vec()
//! );
//! # Ok::<(), compressed::Error>(())
//! ```

use crate::brotli_codec::{BrotliCompress, BrotliDecompress};
use crate::format_macro::define_format;

define_format! {
    name = "brotli",
    encoder_codec = BrotliCompress,
    new_encoder = BrotliCompress::new,
    decoder_codec = BrotliDecompress,
    new_decoder = BrotliDecompress::new,
    concatenated_default = false,
    concatenated_doc = "Sets whether consecutive brotli streams decode as one logical stream.\n\nDisabled by default: brotli has an explicit end-of-stream marker and concatenation is not an established convention.",
}
