//! Output files: files rpi writes so the model can use output it was not
//! shown in full, such as the full text of truncated tool output, binary MCP
//! resources, and images shown by codemode scripts (port of
//! `packages/coding-agent/src/utils/output-files.ts` @ 4a080252f).
//!
//! Every output file is created here, so where they are stored can change in
//! one place. Today they go to the OS temp directory.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Output can carry private data, so only the user may read the files
/// (`OUTPUT_FILE_MODE`).
pub const OUTPUT_FILE_MODE: u32 = 0o600;

/// Attempts before giving up on a unique name; collisions are practically
/// impossible with the 64-bit mix in [`crate::tools::random_hex_16`].
const CREATE_ATTEMPTS: usize = 16;

/// A new, unused path: `<dir>/<prefix>-<random hex><extension>`. `extension`
/// includes the dot (`createOutputFilePath`).
pub fn create_output_file_path(prefix: &str, extension: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}{extension}",
        crate::tools::random_hex_16()
    ))
}

/// Open a new, empty output file; `create_new` never follows a link someone
/// else placed at the path, and the mode keeps it user-only.
fn create_new_file(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(OUTPUT_FILE_MODE);
    }
    options.open(path)
}

/// Write `data` to a new output file and return its path (`writeOutputFile`).
pub fn write_output_file(prefix: &str, extension: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    for _ in 0..CREATE_ATTEMPTS {
        let path = create_output_file_path(prefix, extension);
        match create_new_file(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(data) {
                    // A partial file must not be left behind.
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "could not create a unique output file",
    ))
}

/// Open a new output file for streamed output (`createOutputFileStream`).
pub fn create_output_file_stream(
    prefix: &str,
    extension: &str,
) -> std::io::Result<(PathBuf, File)> {
    for _ in 0..CREATE_ATTEMPTS {
        let path = create_output_file_path(prefix, extension);
        match create_new_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other(
        "could not create a unique output file",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_private_file_and_reports_its_path() {
        let path = write_output_file("rpi-output-files-test", ".txt", b"hello").expect("write");
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(metadata.is_file());
        assert_eq!(std::fs::read(&path).expect("read"), b"hello");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rpi-output-files-test-"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_stream_helper_creates_a_private_file() {
        let (path, mut file) =
            create_output_file_stream("rpi-output-files-test", ".log").expect("open");
        file.write_all(b"stream").expect("write");
        drop(file);
        assert_eq!(std::fs::read(&path).expect("read"), b"stream");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&path);
    }
}
