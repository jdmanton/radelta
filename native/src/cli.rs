//! Argument validation happens before file I/O or pool initialization.
use clap::{Args, Parser, Subcommand, ValueEnum};
use radelta_native::{ContextMode, LossyOptions, Options, DEFAULT_STREAM_MEMORY_MIB};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "radelta",
    version,
    propagate_version = true,
    about = "Compress microscopy TIFFs and restore Radelta files",
    arg_required_else_help = true,
    after_help = "Start with: radelta encode stack.tif\nUse radelta <COMMAND> --help for command-specific options."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
    /// Number of codec workers (otherwise use RAYON_NUM_THREADS or available CPUs)
    #[arg(long, short = 'j', global = true, value_parser = positive_usize, value_name = "N")]
    #[arg(help_heading = "General options")]
    pub threads: Option<usize>,
    /// Suppress progress and success messages; errors are still printed
    #[arg(long, short, global = true)]
    #[arg(help_heading = "General options")]
    pub quiet: bool,
    /// Replace an existing output file (conversion commands only)
    #[arg(long, short, global = true)]
    #[arg(help_heading = "General options")]
    pub force: bool,
    /// Metadata allowance in MiB, separate from image buffers; 0 rejects metadata
    #[arg(long, global = true, default_value = "1024", value_parser = metadata_mib, value_name = "N")]
    #[arg(help_heading = "General options")]
    pub metadata_limit_mib: usize,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Compress uint8/uint16 TIFF data losslessly
    #[command(
        after_help = "Example: radelta encode stack.tif --threads 8\nDefault output: stack.rdlt (input extension replaced). Existing files require --force."
    )]
    Encode(EncodeArgs),
    /// Compress using camera offset and gain; pixel values will change
    #[command(
        after_help = "Example: radelta encode-lossy stack.tif --offset-adu 100 --gain-e-per-adu 0.46\nDefault output: stack.lossy.rdlt. Specify exactly one gain convention.\nPixels will change; the original TIFF is retained."
    )]
    EncodeLossy(LossyEncodeArgs),
    /// Restore a Radelta file to TIFF/BigTIFF with its metadata
    #[command(
        after_help = "Example: radelta decode stack.rdlt -o restored.tif\nDefault output: stack.tif (or remove .rdlt from a .tif.rdlt/.tiff.rdlt name).\nExisting files require --force. Source page order and metadata are restored by default."
    )]
    Decode(DecodeArgs),
    /// Measure lossless codec performance in memory (no output file)
    Benchmark(BenchmarkArgs),
    /// Measure calibrated lossy performance and reconstruction errors
    BenchmarkLossy(LossyBenchmarkArgs),
}

#[derive(Args, Debug)]
pub struct Paths {
    /// Input file (original is retained)
    pub input: PathBuf,
    /// Output path (optional; see default naming below)
    pub output: Option<PathBuf>,
    /// Output path, as an alternative to the positional OUTPUT
    #[arg(
        long = "output",
        short = 'o',
        value_name = "OUTPUT",
        conflicts_with = "output"
    )]
    #[arg(help_heading = "Output options")]
    pub output_option: Option<PathBuf>,
}

impl Paths {
    pub fn resolve(self, extension: &str) -> (PathBuf, PathBuf) {
        let output = self.output.or(self.output_option).unwrap_or_else(|| {
            // Browser output names can already contain the original TIFF suffix.
            if extension == "tif" {
                if let Some(stem) = self.input.file_stem() {
                    let candidate = self.input.with_file_name(stem);
                    if candidate.extension().is_some_and(|ext| {
                        ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff")
                    }) {
                        return candidate;
                    }
                }
            }
            self.input.with_extension(extension)
        });
        (self.input, output)
    }
}

#[derive(Args, Debug)]
pub struct EncodeArgs {
    #[command(flatten)]
    pub paths: Paths,
    #[command(flatten)]
    pub codec: CodecArgs,
    #[command(flatten)]
    pub layout: LayoutArgs,
    #[command(flatten)]
    pub memory: MemoryArgs,
    /// Force bounded-memory streaming, even for a small dataset
    #[arg(long)]
    #[arg(help_heading = "Memory and streaming")]
    pub stream: bool,
}

#[derive(Args, Debug)]
pub struct LossyEncodeArgs {
    #[command(flatten)]
    pub encode: EncodeArgs,
    #[command(flatten)]
    pub calibration: Calibration,
}

#[derive(Args, Debug)]
pub struct DecodeArgs {
    #[command(flatten)]
    pub paths: Paths,
    #[command(flatten)]
    pub memory: MemoryArgs,
    /// Restore pages in this order, slowest to fastest; default: source order
    #[arg(long, value_parser = page_order, value_name = "TCZ")]
    pub page_order: Option<String>,
}

#[derive(Args, Debug)]
pub struct BenchmarkArgs {
    /// TIFF dataset; loaded fully into memory before timing
    pub input: PathBuf,
    #[command(flatten)]
    pub codec: CodecArgs,
    #[command(flatten)]
    pub layout: LayoutArgs,
    /// Measured repetitions after one warm-up; report the median
    #[arg(long, default_value = "3", value_parser = positive_usize, value_name = "N")]
    pub repeats: usize,
}

#[derive(Args, Debug)]
pub struct LossyBenchmarkArgs {
    #[command(flatten)]
    pub benchmark: BenchmarkArgs,
    #[command(flatten)]
    pub calibration: Calibration,
}

#[derive(Args, Debug)]
pub struct MemoryArgs {
    /// Approximate image working-buffer allowance (not a hard process RAM limit)
    #[arg(long, default_value_t = DEFAULT_STREAM_MEMORY_MIB, value_parser = memory_mib, value_name = "N")]
    #[arg(help_heading = "Memory and streaming")]
    pub memory_mib: usize,
}

#[derive(Args, Debug, Default)]
pub struct LayoutArgs {
    /// Override the number of timepoints
    #[arg(long, value_parser = positive_usize, value_name = "N")]
    #[arg(help_heading = "Dataset layout")]
    pub t: Option<usize>,
    /// Override the number of channels
    #[arg(long, value_parser = positive_usize, value_name = "N")]
    #[arg(help_heading = "Dataset layout")]
    pub c: Option<usize>,
    /// Override the number of Z planes
    #[arg(long, value_parser = positive_usize, value_name = "N")]
    #[arg(help_heading = "Dataset layout")]
    pub z: Option<usize>,
    /// Override page axes, slowest to fastest (any permutation of TCZ)
    #[arg(long, value_parser = page_order, value_name = "TCZ")]
    #[arg(help_heading = "Dataset layout")]
    pub page_order: Option<String>,
    /// Ignore OME/ImageJ dimensional metadata when resolving the input layout
    #[arg(long)]
    #[arg(help_heading = "Dataset layout")]
    pub ignore_metadata: bool,
}

#[derive(Args, Debug)]
pub struct CodecArgs {
    /// Z planes per independent codec block
    #[arg(long, default_value = "4", value_parser = clap::value_parser!(u32).range(1..), value_name = "N")]
    #[arg(help_heading = "Codec options")]
    pub block_depth: u32,
    /// Spatial context model
    #[arg(long, value_enum, default_value = "mean")]
    #[arg(help_heading = "Codec options")]
    pub context: Context,
    /// rANS probability precision (8 to 11 bits)
    #[arg(long, default_value = "10", value_parser = clap::value_parser!(u32).range(8..=11), value_name = "N")]
    #[arg(help_heading = "Codec options")]
    pub scale_bits: u32,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Context {
    Mean,
    MeanSigns,
    Signed3,
    Signs,
}

impl CodecArgs {
    pub fn options(&self) -> Options {
        Options {
            block_depth: self.block_depth,
            scale_bits: self.scale_bits,
            context_mode: match self.context {
                Context::Mean => ContextMode::Mean,
                Context::MeanSigns => ContextMode::MeanSigns,
                Context::Signed3 => ContextMode::Signed3,
                Context::Signs => ContextMode::Signs,
            } as u32,
        }
    }
}

#[derive(Args, Debug)]
pub struct Calibration {
    /// Camera offset in ADU (finite; may be negative)
    #[arg(long, value_parser = finite_f64, allow_hyphen_values = true, value_name = "O")]
    #[arg(help_heading = "Lossy calibration")]
    pub offset_adu: f64,
    /// Camera gain in electrons per ADU; specify exactly one gain convention
    #[arg(long, value_parser = positive_f64, required_unless_present = "gain_adu_per_e", conflicts_with = "gain_adu_per_e", value_name = "G")]
    #[arg(help_heading = "Lossy calibration")]
    pub gain_e_per_adu: Option<f64>,
    /// Camera gain in ADU per electron (reciprocal of electrons/ADU)
    #[arg(long, value_parser = reciprocal_gain, required_unless_present = "gain_e_per_adu", value_name = "G")]
    #[arg(help_heading = "Lossy calibration")]
    pub gain_adu_per_e: Option<f64>,
    /// Quantization interval in 2*sqrt(electrons); larger values discard more detail
    #[arg(long, default_value = "2", value_parser = positive_f64, value_name = "D")]
    #[arg(help_heading = "Lossy calibration")]
    pub noise_step: f64,
}

impl Calibration {
    pub fn options(&self, codec: &CodecArgs) -> LossyOptions {
        let o = codec.options();
        LossyOptions {
            block_depth: o.block_depth,
            context_mode: o.context_mode,
            scale_bits: o.scale_bits,
            offset_adu: self.offset_adu,
            gain_e_per_adu: self
                .gain_e_per_adu
                .or(self.gain_adu_per_e)
                .expect("validated gain"),
            noise_step: self.noise_step,
        }
    }
}

fn positive_usize(value: &str) -> Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| "must be a positive integer".into())
}

fn metadata_mib(value: &str) -> Result<usize, String> {
    let n: usize = value.parse().map_err(|_| "must be a nonnegative integer")?;
    n.checked_mul(1024 * 1024)
        .ok_or("MiB value is too large for this platform")?;
    Ok(n)
}

fn memory_mib(value: &str) -> Result<usize, String> {
    let n = metadata_mib(value)?;
    if n == 0 {
        return Err("must be a positive integer".into());
    }
    Ok(n)
}

fn finite_f64(value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| "must be a finite number".into())
}

fn positive_f64(value: &str) -> Result<f64, String> {
    let n = finite_f64(value)?;
    if n <= 0.0 {
        return Err("must be greater than zero".into());
    }
    Ok(n)
}

fn reciprocal_gain(value: &str) -> Result<f64, String> {
    let gain = 1.0 / positive_f64(value)?;
    if !gain.is_finite() {
        return Err("reciprocal gain must be finite".into());
    }
    Ok(gain)
}

fn page_order(value: &str) -> Result<String, String> {
    let order = value.to_ascii_uppercase();
    super::validate_page_order(&order)?;
    Ok(order)
}
