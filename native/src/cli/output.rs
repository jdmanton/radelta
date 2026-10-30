//! Source/output validation and atomic output publication.

use crate::FORCE;
use std::fs;
use std::path::Path;

pub(super) fn prepare_conversion(input: &Path, output: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(input).map_err(|e| format!("cannot read input '{}': {e}", input.display()))?;
    if !metadata.is_file() {
        return Err(format!("input '{}' is not a file", input.display()));
    }
    radelta_native::output::ensure_distinct(input, output)
        .map_err(|e| format!("output '{}': {e}", output.display()))?;
    // symlink_metadata also catches dangling output symlinks.
    match fs::symlink_metadata(output) {
        Ok(m) if m.is_dir() => {
            return Err(format!(
                "output '{}' is a directory; specify a filename",
                output.display()
            ))
        }
        Ok(_) if !FORCE.load(std::sync::atomic::Ordering::Relaxed) => {
            return Err(format!(
                "output '{}' already exists; use --force to replace it or choose another output",
                output.display()
            ))
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("cannot access output '{}': {e}", output.display())),
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.is_dir() {
        return Err(format!(
            "output directory '{}' does not exist or is inaccessible",
            parent.display()
        ));
    }
    Ok(())
}

pub(super) fn publish_output(
    pending: &mut radelta_native::output::PendingOutput,
) -> Result<(), String> {
    if FORCE.load(std::sync::atomic::Ordering::Relaxed) {
        pending.publish()
    } else {
        pending.publish_no_replace()
    }
    .map_err(|e| e.to_string())
}
