//! Native CLI startup, reporting controls and worker-pool selection.

static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
macro_rules! report {
    ($($arg:tt)*) => {
        if !$crate::QUIET.load(std::sync::atomic::Ordering::Relaxed) {
            // A closed report pipe should not panic while processing a file.
            let _ = writeln!(std::io::stdout().lock(), $($arg)*);
        }
    };
}

mod cli;
mod prefetch;
mod tiff_export;
mod tiff_io;
mod tiff_metadata;

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    QUIET.store(cli.quiet, std::sync::atomic::Ordering::Relaxed);
    FORCE.store(cli.force, std::sync::atomic::Ordering::Relaxed);
    radelta_native::set_metadata_limit(cli.metadata_limit_mib * 1024 * 1024);
    let result = if let Some(threads) = cli.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(|e| format!("cannot create codec worker pool: {e}"))
            .and_then(|pool| pool.install(|| cli::run_command(cli.command)))
    } else {
        cli::run_command(cli.command)
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
