//! Sibling temporary files: publish only complete exports, clean up failures.
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OUTPUT: AtomicU64 = AtomicU64::new(0);

/// Reject identical paths, symlinks and hard links to the input.
pub fn ensure_distinct(input: &Path, output: &Path) -> io::Result<()> {
    match fs::metadata(output) {
        Ok(_) if same_file::is_same_file(input, output)? => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input and output must be different files",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Owns the temporary path, independently of the open file. Drop open file
/// handles before this guard so cleanup also works on Windows.
pub struct PendingOutput {
    temporary: PathBuf,
    destination: PathBuf,
    published: bool,
}

impl PendingOutput {
    pub fn create(destination: &Path) -> io::Result<(Self, File)> {
        // Resolve the parent so changes to the working directory do
        // not redirect publication or cleanup. Replace an output symlink itself.
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent.canonicalize()?;
        let name = destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output needs a filename")
        })?;
        let destination = parent.join(name);
        if destination.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output is a directory",
            ));
        }
        loop {
            let number = NEXT_OUTPUT.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(".radelta-{}-{number}.tmp", std::process::id()));
            match OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&temporary)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            temporary,
                            destination,
                            published: false,
                        },
                        file,
                    ))
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.temporary
    }

    /// Call only after all payloads and metadata have been successfully flushed.
    pub fn publish(&mut self) -> io::Result<()> {
        fs::rename(&self.temporary, &self.destination)?;
        self.published = true;
        Ok(())
    }

    /// Publish atomically only if the destination is absent. The sibling
    /// temporary file shares the destination filesystem. Retain cleanup on
    /// Drop in case removing its original hard link fails while a writer is open.
    pub fn publish_no_replace(&mut self) -> io::Result<()> {
        fs::hard_link(&self.temporary, &self.destination).map_err(|e| {
            io::Error::new(e.kind(), format!(
                "cannot publish '{}' without replacing an existing file: {e}; use --force to allow replacement",
                self.destination.display()
            ))
        })?;
        let _ = fs::remove_file(&self.temporary);
        Ok(())
    }
}

impl Drop for PendingOutput {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn no_replace_publication_preserves_a_competing_output_and_cleans_up() {
        let root = std::env::temp_dir().join(format!("radelta-publish-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("result");
        {
            let (mut pending, mut file) = PendingOutput::create(&destination).unwrap();
            file.write_all(b"encoded data").unwrap();
            drop(file);
            fs::write(&destination, b"competing output").unwrap();
            assert_eq!(
                pending.publish_no_replace().unwrap_err().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(fs::read(&destination).unwrap(), b"competing output");
        }
        fs::remove_file(&destination).unwrap();
        {
            let (mut pending, mut file) = PendingOutput::create(&destination).unwrap();
            file.write_all(b"encoded data").unwrap();
            drop(file);
            pending.publish_no_replace().unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"encoded data");
        }
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
