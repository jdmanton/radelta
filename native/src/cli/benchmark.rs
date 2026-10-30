//! In-memory benchmark commands, timing and reconstruction statistics.

use super::layout::DatasetArgs;
use super::reporting::{gb_per_s, mib, print_shape};
use super::tiff_input::{inspect_streaming_dataset, load_dataset};
use radelta_native::{
    compress_lossy_u16_nd_with_stats, compress_u16_nd_with_stats, decompress_u16_nd_into,
    LossyOptions, Options,
};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

// TIFF I/O, output allocation for decoding, and verification are outside timing.
fn measure<T>(
    repeats: usize,
    mut run: impl FnMut() -> Result<T, String>,
) -> Result<(T, f64), String> {
    drop(run()?);
    let mut times = Vec::with_capacity(repeats);
    let mut result = None;
    for _ in 0..repeats {
        let start = Instant::now();
        let value = run()?;
        times.push(start.elapsed().as_secs_f64());
        result = Some(value);
    }
    times.sort_by(f64::total_cmp);
    let median = (times[(repeats - 1) / 2] + times[repeats / 2]) / 2.0;
    Ok((result.unwrap(), median))
}

pub(super) fn benchmark(
    path: &Path,
    opts: Options,
    ds: DatasetArgs,
    repeats: usize,
) -> Result<(), String> {
    let io0 = Instant::now();
    let (data, layout) = load_dataset(path, &ds)?;
    let dims = layout.dims;
    let raw_bytes = data.len() * inspect_streaming_dataset(path, &ds)?.sample_bytes;
    report!("Input:           {}", path.display());
    report!("Codec options:   {opts:?}");
    report!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    report!("Raw bytes:       {:.3} MiB", mib(raw_bytes));
    report!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    report!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    report!("Lossless compression:");
    report!("  volumes (T*C): {}", stats.volumes);
    report!("  q max:         {}", stats.q_max);
    report!("  r max:         {}", stats.r_max);
    report!("  blocks:        {}", stats.blocks);
    report!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    report!("  r stream:      {:.3} MiB", mib(stats.r_stream_bytes));
    report!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    report!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    report!(
        "  ratio:         {:.2}:1",
        raw_bytes as f64 / stats.total_bytes as f64
    );
    report!("  encode time:   {:.4} s", encode_time);
    report!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims || decoded != data {
        return Err("round-trip verification FAILED".into());
    }
    report!();
    report!("Decompression:");
    report!("  decode time:   {:.4} s", ds_time);
    report!("  decode rate:   {:.3} GB/s", gb_per_s(raw_bytes, ds_time));
    report!("  exact:         yes");
    Ok(())
}

fn error_stats(original: &[u16], decoded: &[u16], opts: LossyOptions) -> (f64, f64, u16, f64) {
    let mut sse = 0.0f64;
    let mut sae = 0.0f64;
    let mut max_abs = 0u16;
    let mut sse_e = 0.0f64;
    for (&a, &b) in original.iter().zip(decoded.iter()) {
        let d = b as f64 - a as f64;
        sse += d * d;
        sae += d.abs();
        max_abs = max_abs.max((b as i32 - a as i32).unsigned_abs().min(u16::MAX as u32) as u16);
        let ea = ((a as f64 - opts.offset_adu) * opts.gain_e_per_adu).max(0.0);
        let eb = ((b as f64 - opts.offset_adu) * opts.gain_e_per_adu).max(0.0);
        let de = eb - ea;
        sse_e += de * de;
    }
    let n = original.len().max(1) as f64;
    ((sse / n).sqrt(), sae / n, max_abs, (sse_e / n).sqrt())
}

pub(super) fn benchmark_lossy(
    path: &Path,
    opts: LossyOptions,
    ds: DatasetArgs,
    repeats: usize,
) -> Result<(), String> {
    let io0 = Instant::now();
    let (data, layout) = load_dataset(path, &ds)?;
    let dims = layout.dims;
    let raw_bytes = data.len() * inspect_streaming_dataset(path, &ds)?.sample_bytes;
    report!("Input:           {}", path.display());
    report!("Codec options:   {opts:?}");
    report!(
        "Timing:          median of {repeats} runs after warm-up, {} workers",
        rayon::current_num_threads()
    );
    print_shape(&layout);
    report!("Raw bytes:       {:.3} MiB", mib(raw_bytes));
    report!("TIFF read/map:   {:.3} s", io0.elapsed().as_secs_f64());
    report!();
    report!("Lossy calibration:");
    report!("  offset:        {:.6} ADU", opts.offset_adu);
    report!("  gain:          {:.6} e-/ADU", opts.gain_e_per_adu);
    report!("  noise step:    {:.6}", opts.noise_step);
    report!(
        "  quant RMS*:    {:.4} sigma",
        opts.noise_step / 12.0f64.sqrt()
    );
    report!();

    let ((encoded, stats), encode_time) = measure(repeats, || {
        compress_lossy_u16_nd_with_stats(&data, dims, opts).map_err(|e| e.to_string())
    })?;
    report!("Lossy compression:");
    report!("  volumes (T*C): {}", stats.volumes);
    report!("  q max:         {}", stats.q_max);
    report!("  blocks:        {}", stats.blocks);
    report!("  q stream:      {:.3} MiB", mib(stats.q_stream_bytes));
    report!("  model tables:  {:.3} MiB", mib(stats.model_bytes));
    report!("  total file:    {:.3} MiB", mib(stats.total_bytes));
    report!(
        "  ratio:         {:.2}:1",
        raw_bytes as f64 / stats.total_bytes as f64
    );
    report!("  encode time:   {:.4} s", encode_time);
    report!(
        "  encode rate:   {:.3} GB/s",
        gb_per_s(raw_bytes, encode_time)
    );

    let mut decoded = vec![0u16; data.len()];
    let (dd, ds_time) = measure(repeats, || {
        decompress_u16_nd_into(&encoded, &mut decoded).map_err(|e| e.to_string())
    })?;
    if dd != dims {
        return Err("lossy decoded dimensions do not match".into());
    }
    let (rmse, mae, max_abs, rmse_e) = error_stats(&data, &decoded, opts);
    report!();
    report!("Decompression:");
    report!("  decode time:   {:.4} s", ds_time);
    report!("  decode rate:   {:.3} GB/s", gb_per_s(raw_bytes, ds_time));
    report!("  RMSE:          {:.4} ADU", rmse);
    report!("  MAE:           {:.4} ADU", mae);
    report!("  max abs error: {} ADU", max_abs);
    report!("  RMSE signal:   {:.4} electrons", rmse_e);
    Ok(())
}
