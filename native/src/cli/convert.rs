//! Lossless/lossy conversion and TIFF restoration.

use super::layout::{DatasetArgs, ResolvedLayout};
use super::output::publish_output;
use super::reporting::{gb_per_s, mib};
use super::streaming::{encode_streaming_lossless, encode_streaming_lossy};
use super::tiff_input::{inspect_streaming_dataset, load_dataset};
use crate::{tiff_export, tiff_metadata};
use radelta_native::{
    compress_lossy_u16_nd_with_stats, compress_u16_nd_with_stats, Dims5, LossyOptions, Options,
};
use std::io::Write;
use std::path::Path;

fn write_encoded_with_metadata(
    input: &Path,
    output: &Path,
    encoded: &[u8],
    layout: &ResolvedLayout,
) -> Result<usize, String> {
    let metadata = tiff_metadata::capture(input, layout)?;
    let (mut pending, mut file) =
        radelta_native::output::PendingOutput::create(output).map_err(|e| e.to_string())?;
    let written = radelta_native::write_encoded_with_metadata(&mut file, encoded, &metadata)
        .map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    drop(file);
    publish_output(&mut pending)?;
    Ok(written)
}

pub(super) fn encode_lossless(
    input: &Path,
    output: &Path,
    opts: Options,
    ds: DatasetArgs,
) -> Result<(), String> {
    match inspect_streaming_dataset(input, &ds) {
        Ok(info) => {
            let d = info.layout.dims;
            let raw =
                d.x.checked_mul(d.y)
                    .and_then(|v| v.checked_mul(d.z))
                    .and_then(|v| v.checked_mul(d.c))
                    .and_then(|v| v.checked_mul(d.t))
                    .and_then(|v| v.checked_mul(2))
                    .ok_or_else(|| "dataset size overflow".to_string());
            match raw {
                Ok(raw_bytes) => {
                    let budget = ds.memory_mib.saturating_mul(1024 * 1024);
                    let use_stream = ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                    if use_stream {
                        report!(
                            "Using out-of-core RDS3 encoding ({}; RAM allowance {} MiB).",
                            if ds.force_stream {
                                "requested with --stream"
                            } else {
                                "estimated working set exceeds allowance"
                            },
                            ds.memory_mib
                        );
                        encode_streaming_lossless(input, output, opts, &ds, &info)
                    } else {
                        match load_dataset(input, &ds) {
                            Ok((data, layout)) => {
                                let dims = layout.dims;
                                match compress_u16_nd_with_stats(&data, dims, opts) {
                                    Ok((encoded, stats)) => {
                                        match write_encoded_with_metadata(
                                            input, output, &encoded, &layout,
                                        ) {
                                            Err(e) => Err(e),
                                            Ok(written) => {
                                                report!(
                                                    "Wrote {} ({:.3} MiB)",
                                                    output.display(),
                                                    mib(written)
                                                );
                                                report!(
                                                    "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                    dims.t,
                                                    dims.c,
                                                    dims.z,
                                                    dims.y,
                                                    dims.x
                                                );
                                                report!("Layout source: {}", layout.source);
                                                report!(
                                                    "Lossless encode rate: {:.3} GB/s",
                                                    gb_per_s(
                                                        stats.raw_bytes / 2 * info.sample_bytes,
                                                        stats.seconds_total
                                                    )
                                                );
                                                Ok(())
                                            }
                                        }
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

pub(super) fn encode_lossy(
    input: &Path,
    output: &Path,
    opts: LossyOptions,
    ds: DatasetArgs,
) -> Result<(), String> {
    match inspect_streaming_dataset(input, &ds) {
        Ok(info) => {
            let d = info.layout.dims;
            let raw =
                d.x.checked_mul(d.y)
                    .and_then(|v| v.checked_mul(d.z))
                    .and_then(|v| v.checked_mul(d.c))
                    .and_then(|v| v.checked_mul(d.t))
                    .and_then(|v| v.checked_mul(2))
                    .ok_or_else(|| "dataset size overflow".to_string());
            match raw {
                Ok(raw_bytes) => {
                    let budget = ds.memory_mib.saturating_mul(1024 * 1024);
                    let use_stream = ds.force_stream || raw_bytes.saturating_mul(4) > budget;
                    if use_stream {
                        report!(
                            "Using out-of-core RQS3 encoding ({}; RAM allowance {} MiB).",
                            if ds.force_stream {
                                "requested with --stream"
                            } else {
                                "estimated working set exceeds allowance"
                            },
                            ds.memory_mib
                        );
                        encode_streaming_lossy(input, output, opts, &ds, &info)
                    } else {
                        match load_dataset(input, &ds) {
                            Ok((data, layout)) => {
                                let dims = layout.dims;
                                match compress_lossy_u16_nd_with_stats(&data, dims, opts) {
                                    Ok((encoded, stats)) => {
                                        match write_encoded_with_metadata(
                                            input, output, &encoded, &layout,
                                        ) {
                                            Err(e) => Err(e),
                                            Ok(written) => {
                                                report!(
                                                    "Wrote {} ({:.3} MiB)",
                                                    output.display(),
                                                    mib(written)
                                                );
                                                report!(
                                                    "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
                                                    dims.t,
                                                    dims.c,
                                                    dims.z,
                                                    dims.y,
                                                    dims.x
                                                );
                                                report!("Layout source: {}", layout.source);
                                                report!(
                                                    "Lossy encode rate: {:.3} GB/s",
                                                    gb_per_s(
                                                        stats.raw_bytes / 2 * info.sample_bytes,
                                                        stats.seconds_total
                                                    )
                                                );
                                                report!("offset={:.6} ADU, gain={:.6} e-/ADU, noise-step={:.6}", opts.offset_adu, opts.gain_e_per_adu, opts.noise_step);
                                                Ok(())
                                            }
                                        }
                                    }
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

pub(super) fn decode_file(
    input: &Path,
    output: &Path,
    order: Option<String>,
    memory_mib: usize,
) -> Result<(), String> {
    radelta_native::RadeltaFileHandle::open(input).and_then(|file| {
        let info = file.info();
        let d = Dims5 {
            x: info.nx as usize,
            y: info.ny as usize,
            z: info.nz as usize,
            c: info.nc as usize,
            t: info.nt as usize,
        };
        let (mut pending, reserved) =
            radelta_native::output::PendingOutput::create(output).map_err(|e| e.to_string())?;
        drop(reserved);
        let metadata_len = file.metadata().len();
        tiff_export::write(
            std::sync::Arc::new(file),
            d,
            order.as_deref(),
            memory_mib,
            pending.path(),
        )?;
        publish_output(&mut pending)?;
        report!("Wrote {}", output.display());
        report!(
            "Shape T,C,Z,Y,X: {}, {}, {}, {}, {}",
            d.t,
            d.c,
            d.z,
            d.y,
            d.x
        );
        report!("Metadata: {} bytes", metadata_len);
        Ok(())
    })
}
