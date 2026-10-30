//! High-throughput codec for uint16 microscopy images.
//!
//! Supports XYZ volumes and canonical contiguous TCZYX datasets.
//!
//! Lossless representation:
//!     q = floor(sqrt(x))
//!     r = x - q*q
//!
//! Lossy representation (camera-calibrated):
//!     electrons = max((ADU - offset_adu) * gain_e_per_adu, 0)
//!     z = 2*sqrt(electrons)
//!     q = round(z / noise_step)
//!
//! Lossless q is coded with a compact causal spatial context and r with P(r|q).
//! Lossy mode omits r entirely and codes only the calibrated/quantised q stream.
//! Static probability tables are learned in a first pass, then independent
//! Z-slabs are encoded in parallel with 4-way interleaved byte-rANS.

mod codec;
mod container;
mod format;
mod types;

pub use codec::{
    compress_lossy_u16, compress_lossy_u16_with_stats, compress_u16, compress_u16_with_stats,
    decompress_u16, decompress_u16_into, inspect_dims,
};
pub use container::{
    compress_lossy_u16_nd, compress_lossy_u16_nd_with_stats, compress_u16_nd,
    compress_u16_nd_with_stats, decompress_u16_nd, decompress_u16_nd_into, inspect_shape,
};
#[doc(hidden)]
pub use format::{STREAM_MAGIC_LOSSLESS, STREAM_MAGIC_LOSSY, STREAM_VERSION};
pub use types::{
    choose_stream_chunk_depth, CompressionStats, ContextMode, Dims, Dims5, LossyOptions,
    NDCompressionStats, Options, RadeltaError, Result, DEFAULT_BLOCK_DEPTH,
    DEFAULT_STREAM_MEMORY_MIB,
};

pub mod metadata;
#[doc(hidden)]
pub mod output;
pub use metadata::{
    metadata_limit, read_file_metadata, read_metadata, set_file_metadata, set_metadata,
    set_metadata_limit, write_encoded_with_metadata, DEFAULT_METADATA_LIMIT_BYTES,
};
mod ffi;
mod file_api;

pub use ffi::{
    radelta_compress_lossy_u16, radelta_compress_lossy_u16_nd, radelta_compress_u16,
    radelta_compress_u16_nd, radelta_decompress_u16, radelta_decompress_u16_nd,
    radelta_get_metadata, radelta_get_metadata_limit, radelta_set_metadata,
    radelta_set_metadata_limit, RADELTA_BUFFER_TOO_SMALL, RADELTA_CODEC_ERROR,
    RADELTA_INVALID_ARGUMENT, RADELTA_OK,
};
pub use file_api::{
    RadeltaDecodeUnit, RadeltaFileHandle, RadeltaFileInfo, RadeltaStreamWriter,
    DEFAULT_FILE_CACHE_BYTES,
};
