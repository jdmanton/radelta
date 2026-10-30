//! Pixel transforms, statistical models and independent block coding.

// Dispatch once per block, never once per voxel. Default kernels expose the
// mean context and ten-bit rANS precision as compile-time constants; the
// general kernels retain every other supported context/precision combination.
macro_rules! default_kernel {
    ($default:expr, $kernel:ident($($args:expr),* $(,)?)) => {
        if $default { $kernel::<true>($($args),*) } else { $kernel::<false>($($args),*) }
    };
}

const LANES: usize = 4;
const RANS_L: u32 = 1 << 23;

mod block;
mod context;
mod decode;
mod encode;
mod model;
mod transform;

pub use encode::{
    compress_lossy_u16, compress_lossy_u16_with_stats, compress_u16, compress_u16_with_stats,
};

pub use decode::{decompress_u16, decompress_u16_into, inspect_dims};

pub(crate) use decode::{
    alloc_zeroed_u16, decompress_u16_expecting, inspect_lossless_dims, inspect_lossy_dims,
};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use transform::{lossy_quantize, lossy_reconstruct};
