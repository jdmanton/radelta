//! TCZYX containers holding independent channel/timepoint streams.

use crate::codec::{
    alloc_zeroed_u16, compress_lossy_u16_with_stats, compress_u16_with_stats, decompress_u16_into,
    inspect_dims, inspect_lossless_dims, inspect_lossy_dims,
};
use crate::format::{
    put_u16, put_u32, put_u64, Reader, MAGIC, MAGIC_LOSSY, MAGIC_ND, MAGIC_ND_LOSSY, ND_VERSION,
};
use crate::metadata;
use crate::types::{
    CompressionStats, Dims5, LossyOptions, NDCompressionStats, Options, RadeltaError, Result,
};
use rayon::prelude::*;
use std::time::Instant;

/// Compress canonical contiguous T,C,Z,Y,X uint16 data losslessly.
/// Each (t,c) XYZ volume is coded independently using an RDL1
/// codec and wrapped in an RDM2 multidimensional container.
pub fn compress_u16_nd(input: &[u16], dims: Dims5, options: Options) -> Result<Vec<u8>> {
    compress_u16_nd_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_u16_nd_with_stats(
    input: &[u16],
    dims: Dims5,
    options: Options,
) -> Result<(Vec<u8>, NDCompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let expected = dims.voxels()?;
    if input.len() != expected {
        return Err(RadeltaError(format!(
            "input length {} does not match TCZYX dimensions ({expected} voxels)",
            input.len()
        )));
    }
    let volume_voxels = dims.volume_voxels()?;
    let nvol = dims.volumes()?;
    let d3 = dims.volume_dims();

    let results: Vec<Result<(Vec<u8>, CompressionStats)>> = input
        .par_chunks(volume_voxels)
        .map(|vol| compress_u16_with_stats(vol, d3, options))
        .collect();

    let mut streams = Vec::with_capacity(nvol);
    let mut q_stream_bytes = 0usize;
    let mut r_stream_bytes = 0usize;
    let mut model_bytes = 0usize;
    let mut blocks = 0usize;
    let mut q_max = 0u8;
    let mut r_max = 0u16;
    for res in results {
        let (stream, st) = res?;
        q_stream_bytes += st.q_stream_bytes;
        r_stream_bytes += st.r_stream_bytes;
        model_bytes += st.model_bytes;
        blocks += st.blocks;
        q_max = q_max.max(st.q_max);
        r_max = r_max.max(st.r_max);
        streams.push(stream);
    }
    if streams.len() != nvol {
        return Err(RadeltaError(
            "internal multidimensional stream count mismatch".into(),
        ));
    }

    let payload_bytes: usize = streams.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(payload_bytes + 64 + nvol * 8);
    out.extend_from_slice(MAGIC_ND);
    put_u16(&mut out, ND_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, dims.c as u32);
    put_u32(&mut out, dims.t as u32);
    put_u32(&mut out, nvol as u32);
    for stream in &streams {
        put_u64(&mut out, stream.len() as u64);
        out.extend_from_slice(stream);
    }

    let stats = NDCompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes,
        model_bytes,
        blocks,
        volumes: nvol,
        q_max,
        r_max,
        seconds_total: total_start.elapsed().as_secs_f64(),
    };
    Ok((out, stats))
}

/// Compress canonical contiguous T,C,Z,Y,X uint16 data using calibrated lossy mode.
/// Each (t,c) XYZ volume is coded independently using RDLQ and wrapped in RDQ2.
pub fn compress_lossy_u16_nd(input: &[u16], dims: Dims5, options: LossyOptions) -> Result<Vec<u8>> {
    compress_lossy_u16_nd_with_stats(input, dims, options).map(|v| v.0)
}

pub fn compress_lossy_u16_nd_with_stats(
    input: &[u16],
    dims: Dims5,
    options: LossyOptions,
) -> Result<(Vec<u8>, NDCompressionStats)> {
    let total_start = Instant::now();
    dims.validate()?;
    let expected = dims.voxels()?;
    if input.len() != expected {
        return Err(RadeltaError(format!(
            "input length {} does not match TCZYX dimensions ({expected} voxels)",
            input.len()
        )));
    }
    let volume_voxels = dims.volume_voxels()?;
    let nvol = dims.volumes()?;
    let d3 = dims.volume_dims();

    let results: Vec<Result<(Vec<u8>, CompressionStats)>> = input
        .par_chunks(volume_voxels)
        .map(|vol| compress_lossy_u16_with_stats(vol, d3, options))
        .collect();

    let mut streams = Vec::with_capacity(nvol);
    let mut q_stream_bytes = 0usize;
    let mut model_bytes = 0usize;
    let mut blocks = 0usize;
    let mut q_max = 0u8;
    for res in results {
        let (stream, st) = res?;
        q_stream_bytes += st.q_stream_bytes;
        model_bytes += st.model_bytes;
        blocks += st.blocks;
        q_max = q_max.max(st.q_max);
        streams.push(stream);
    }

    let payload_bytes: usize = streams.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(payload_bytes + 64 + nvol * 8);
    out.extend_from_slice(MAGIC_ND_LOSSY);
    put_u16(&mut out, ND_VERSION);
    put_u16(&mut out, 0);
    put_u32(&mut out, dims.x as u32);
    put_u32(&mut out, dims.y as u32);
    put_u32(&mut out, dims.z as u32);
    put_u32(&mut out, dims.c as u32);
    put_u32(&mut out, dims.t as u32);
    put_u32(&mut out, nvol as u32);
    for stream in &streams {
        put_u64(&mut out, stream.len() as u64);
        out.extend_from_slice(stream);
    }

    let stats = NDCompressionStats {
        raw_bytes: input.len() * 2,
        total_bytes: out.len(),
        q_stream_bytes,
        r_stream_bytes: 0,
        model_bytes,
        blocks,
        volumes: nvol,
        q_max,
        r_max: 0,
        seconds_total: total_start.elapsed().as_secs_f64(),
    };
    Ok((out, stats))
}

fn inspect_nd_header(data: &[u8], expected_magic: &[u8; 4]) -> Result<(Dims5, usize, usize)> {
    let mut rd = Reader::new(data);
    if rd.take(4)? != expected_magic {
        return Err(RadeltaError(
            "not the requested multidimensional Radelta stream".into(),
        ));
    }
    let version = rd.u16()?;
    if version != ND_VERSION {
        return Err(RadeltaError(format!(
            "unsupported multidimensional Radelta version {version}"
        )));
    }
    let _flags = rd.u16()?;
    let dims = Dims5 {
        x: rd.u32()? as usize,
        y: rd.u32()? as usize,
        z: rd.u32()? as usize,
        c: rd.u32()? as usize,
        t: rd.u32()? as usize,
    };
    dims.validate()?;
    let nvol = rd.u32()? as usize;
    if nvol != dims.volumes()? {
        return Err(RadeltaError(
            "multidimensional volume count does not match T*C".into(),
        ));
    }
    Ok((dims, nvol, rd.pos))
}

fn decompress_nd_container_into(
    data: &[u8],
    magic: &[u8; 4],
    lossy: bool,
    output: &mut [u16],
) -> Result<Dims5> {
    let (data, _) = metadata::split(data)?;
    let (dims, nvol, header_pos) = inspect_nd_header(data, magic)?;
    let needed = dims.voxels()?;
    if output.len() != needed {
        return Err(RadeltaError(format!(
            "output length {} does not match decoded voxel count {needed}",
            output.len()
        )));
    }

    let mut rd = Reader {
        data,
        pos: header_pos,
    };
    if nvol > data.len().saturating_sub(header_pos) / 28 {
        return Err(RadeltaError(
            "multidimensional volume count exceeds available records".into(),
        ));
    }
    let mut slices: Vec<&[u8]> = Vec::new();
    slices
        .try_reserve_exact(nvol)
        .map_err(|_| RadeltaError("cannot allocate multidimensional volume index".into()))?;
    for _ in 0..nvol {
        let len64 = rd.u64()?;
        let len = usize::try_from(len64)
            .map_err(|_| RadeltaError("substream length exceeds usize".into()))?;
        let sub = rd.take(len)?;
        if sub.len() < 4 {
            return Err(RadeltaError("truncated multidimensional substream".into()));
        }
        let expected = if lossy { MAGIC_LOSSY } else { MAGIC };
        if &sub[..4] != expected {
            return Err(RadeltaError(
                "multidimensional substream mode mismatch".into(),
            ));
        }
        slices.push(sub);
    }
    if !rd.is_done() {
        return Err(RadeltaError(
            "trailing bytes after multidimensional payload".into(),
        ));
    }

    let d3 = dims.volume_dims();
    let volume_voxels = d3.voxels()?;
    output
        .par_chunks_mut(volume_voxels)
        .zip(slices.par_iter())
        .try_for_each(|(chunk, sub)| {
            let sd = decompress_u16_into(sub, chunk)?;
            if sd != d3 {
                return Err(RadeltaError(
                    "multidimensional substream dimensions do not match container".into(),
                ));
            }
            Ok(())
        })?;

    Ok(dims)
}

/// Decode RDL1/RDLQ/RDM2/RDQ2 data directly into canonical TCZYX storage.
pub fn decompress_u16_nd_into(data: &[u8], output: &mut [u16]) -> Result<Dims5> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC_ND {
        decompress_nd_container_into(data, MAGIC_ND, false, output)
    } else if &data[..4] == MAGIC_ND_LOSSY {
        decompress_nd_container_into(data, MAGIC_ND_LOSSY, true, output)
    } else if &data[..4] == MAGIC || &data[..4] == MAGIC_LOSSY {
        let d = inspect_dims(data)?;
        if output.len() != d.voxels()? {
            return Err(RadeltaError(format!(
                "output length {} does not match decoded voxel count {}",
                output.len(),
                d.voxels()?
            )));
        }
        let decoded = decompress_u16_into(data, output)?;
        Ok(Dims5 {
            x: decoded.x,
            y: decoded.y,
            z: decoded.z,
            c: 1,
            t: 1,
        })
    } else {
        Err(RadeltaError(
            "not a Radelta RDL1/RDLQ/RDM2/RDQ2 stream".into(),
        ))
    }
}

/// Decode RDL1/RDLQ or multidimensional RDM2/RDQ2 data into canonical TCZYX.
pub fn decompress_u16_nd(data: &[u8]) -> Result<(Vec<u16>, Dims5)> {
    let dims = inspect_shape(data)?;
    let mut out = alloc_zeroed_u16(dims.voxels()?)?;
    let decoded_dims = decompress_u16_nd_into(data, &mut out)?;
    Ok((out, decoded_dims))
}

pub fn inspect_shape(data: &[u8]) -> Result<Dims5> {
    if data.len() < 4 {
        return Err(RadeltaError("truncated input".into()));
    }
    if &data[..4] == MAGIC_ND {
        inspect_nd_header(data, MAGIC_ND).map(|v| v.0)
    } else if &data[..4] == MAGIC_ND_LOSSY {
        inspect_nd_header(data, MAGIC_ND_LOSSY).map(|v| v.0)
    } else if &data[..4] == MAGIC {
        inspect_lossless_dims(data).map(|d| Dims5 {
            x: d.x,
            y: d.y,
            z: d.z,
            c: 1,
            t: 1,
        })
    } else if &data[..4] == MAGIC_LOSSY {
        inspect_lossy_dims(data).map(|d| Dims5 {
            x: d.x,
            y: d.y,
            z: d.z,
            c: 1,
            t: 1,
        })
    } else {
        Err(RadeltaError("not a Radelta stream".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::test_support::golden_rdm2_expected;
    use crate::codec::{lossy_quantize, lossy_reconstruct};
    use crate::types::{Dims5, LossyOptions, Options};

    #[test]
    fn nd_lossless_roundtrip_rectangular_tczyx() {
        let dims = Dims5 {
            x: 17,
            y: 9,
            z: 4,
            c: 3,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n)
            .map(|i| ((i * 37 + (i / 17) * 11) % 4096) as u16)
            .collect();
        let enc = compress_u16_nd(&input, dims, Options::default()).unwrap();
        let (dec, dd) = decompress_u16_nd(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec, input);
    }

    #[test]
    fn nd_lossy_preserves_shape_and_rectangular_xy() {
        let dims = Dims5 {
            x: 13,
            y: 7,
            z: 3,
            c: 2,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n).map(|i| ((i * 19) % 3000) as u16).collect();
        let opts = LossyOptions {
            offset_adu: 0.0,
            gain_e_per_adu: 0.46,
            noise_step: 2.0,
            ..LossyOptions::default()
        };
        let enc = compress_lossy_u16_nd(&input, dims, opts).unwrap();
        let (dec, dd) = decompress_u16_nd(&enc).unwrap();
        assert_eq!(dd, dims);
        assert_eq!(dec.len(), input.len());
        let mut direct = vec![u16::MAX; n];
        assert_eq!(decompress_u16_nd_into(&enc, &mut direct).unwrap(), dims);
        for (&value, &actual) in input.iter().zip(&direct) {
            let q = lossy_quantize(value, opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
            assert_eq!(
                actual,
                lossy_reconstruct(
                    q as u8,
                    opts.offset_adu,
                    opts.gain_e_per_adu,
                    opts.noise_step
                )
            );
        }
        assert_eq!(direct, dec);
    }

    #[test]
    fn golden_rdm2_v1_decodes() {
        let encoded = include_bytes!("../tests/fixtures/rdm2_reference_v1.rdlt");
        let (expected, dims) = golden_rdm2_expected();
        let (decoded, decoded_dims) = decompress_u16_nd(encoded).unwrap();
        assert_eq!(decoded_dims, dims);
        assert_eq!(decoded, expected);
        assert_eq!(inspect_shape(encoded).unwrap(), dims);
    }

    #[test]
    fn nd_container_rejects_trailing_bytes() {
        let dims = Dims5 {
            x: 6,
            y: 5,
            z: 3,
            c: 2,
            t: 2,
        };
        let n = dims.voxels().unwrap();
        let input: Vec<u16> = (0..n).map(|i| ((i * 97 + i / 11) % 65536) as u16).collect();
        let mut encoded = compress_u16_nd(&input, dims, Options::default()).unwrap();
        encoded.push(0);
        assert!(decompress_u16_nd(&encoded).is_err());
    }
}
