//! Command dispatch and conversion of parsed arguments to dataset settings.

mod args;
mod benchmark;
mod convert;
pub(crate) mod layout;
mod output;
mod reporting;
mod streaming;
pub(crate) mod tiff_input;

pub use args::Cli;

use self::benchmark::{benchmark, benchmark_lossy};
use self::convert::{decode_file, encode_lossless, encode_lossy};
use self::layout::DatasetArgs;
use self::output::prepare_conversion;
use radelta_native::DEFAULT_STREAM_MEMORY_MIB;

fn dataset(layout: args::LayoutArgs, memory_mib: usize, force_stream: bool) -> DatasetArgs {
    DatasetArgs {
        t: layout.t,
        c: layout.c,
        z: layout.z,
        page_order: layout.page_order,
        ignore_metadata: layout.ignore_metadata,
        memory_mib,
        force_stream,
    }
}

pub(super) fn run_command(command: args::Command) -> Result<(), String> {
    use args::Command::*;
    match command {
        Encode(args) => {
            let (input, output) = args.paths.resolve("rdlt");
            prepare_conversion(&input, &output)?;
            encode_lossless(
                &input,
                &output,
                args.codec.options(),
                dataset(args.layout, args.memory.memory_mib, args.stream),
            )
            .map_err(|e| {
                format!(
                    "cannot encode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        EncodeLossy(args) => {
            let opts = args.calibration.options(&args.encode.codec);
            let args = args.encode;
            let (input, output) = args.paths.resolve("lossy.rdlt");
            prepare_conversion(&input, &output)?;
            encode_lossy(
                &input,
                &output,
                opts,
                dataset(args.layout, args.memory.memory_mib, args.stream),
            )
            .map_err(|e| {
                format!(
                    "cannot encode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        Decode(args) => {
            let (input, output) = args.paths.resolve("tif");
            prepare_conversion(&input, &output)?;
            decode_file(&input, &output, args.page_order, args.memory.memory_mib).map_err(|e| {
                format!(
                    "cannot decode '{}' to '{}': {e}",
                    input.display(),
                    output.display()
                )
            })
        }
        Benchmark(args) => benchmark(
            &args.input,
            args.codec.options(),
            dataset(args.layout, DEFAULT_STREAM_MEMORY_MIB, false),
            args.repeats,
        )
        .map_err(|e| format!("cannot benchmark '{}': {e}", args.input.display())),
        BenchmarkLossy(args) => {
            let opts = args.calibration.options(&args.benchmark.codec);
            let args = args.benchmark;
            benchmark_lossy(
                &args.input,
                opts,
                dataset(args.layout, DEFAULT_STREAM_MEMORY_MIB, false),
                args.repeats,
            )
            .map_err(|e| format!("cannot benchmark '{}': {e}", args.input.display()))
        }
    }
}
