//! Process `@file` CLI arguments into text content and image attachments.
//!
//! Port of `packages/coding-agent/src/cli/file-processor.ts` @ pi 0.82.1
//! (2efa728).
//!
//! Upstream prints to stderr and `process.exit(1)` on missing/unreadable
//! files; here [`process_file_arguments`] returns [`FileProcessError`] and the
//! app layer prints `Error: {error}` and exits 1 (same user-visible result).

use std::path::{Path, PathBuf};

use rpi_ai::types::ImageContent;
use thiserror::Error;

use crate::tools::image_process::process_image;
use crate::tools::mime::{detect_supported_image_mime_type, IMAGE_TYPE_SNIFF_BYTES};
use crate::tools::path_utils::resolve_read_path;

/// `ProcessedFiles` (file-processor.ts:13-16).
#[derive(Debug, Default)]
pub struct ProcessedFiles {
    pub text: String,
    pub images: Vec<ImageContent>,
}

/// Terminal processing failure — upstream exits 1 with this message on
/// stderr (file-processor.ts:37-39, :78-82).
#[derive(Debug, Error)]
pub enum FileProcessError {
    #[error("File not found: {}", .0.display())]
    FileNotFound(PathBuf),
    #[error("Could not read file {}: {message}", .path.display())]
    ReadFailed { path: PathBuf, message: String },
}

/// Sniff the leading bytes for a supported image MIME type
/// (`detectSupportedImageMimeTypeFromFile`, utils/mime.ts).
async fn detect_image_mime_from_file(path: &Path) -> Option<&'static str> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buffer = vec![0u8; IMAGE_TYPE_SNIFF_BYTES];
    let read = file.read(&mut buffer).await.ok()?;
    buffer.truncate(read);
    detect_supported_image_mime_type(&buffer)
}

/// `processFileArguments` (file-processor.ts:24-87). `auto_resize_images`
/// defaults to true upstream (`options?.autoResizeImages ?? true`).
pub async fn process_file_arguments(
    file_args: &[String],
    cwd: &Path,
    auto_resize_images: bool,
) -> Result<ProcessedFiles, FileProcessError> {
    let mut text = String::new();
    let mut images = Vec::new();

    for file_arg in file_args {
        // Expand and resolve path (handles ~ expansion and macOS screenshot
        // Unicode spaces).
        let absolute_path = resolve_read_path(file_arg, cwd).await;

        // Check if file exists.
        if !tokio::fs::try_exists(&absolute_path).await.unwrap_or(false) {
            return Err(FileProcessError::FileNotFound(absolute_path));
        }

        // Skip empty files.
        let stats = tokio::fs::metadata(&absolute_path).await.map_err(|e| {
            FileProcessError::ReadFailed {
                path: absolute_path.clone(),
                message: e.to_string(),
            }
        })?;
        if stats.len() == 0 {
            continue;
        }

        let mime_type = detect_image_mime_from_file(&absolute_path).await;

        if let Some(mime_type) = mime_type {
            // Handle image file.
            let content = tokio::fs::read(&absolute_path).await.map_err(|e| {
                FileProcessError::ReadFailed {
                    path: absolute_path.clone(),
                    message: e.to_string(),
                }
            })?;
            match process_image(&content, mime_type, auto_resize_images) {
                Err(processed) => {
                    text.push_str(&format!(
                        "<file name=\"{}\">{}</file>\n",
                        absolute_path.display(),
                        processed.message
                    ));
                }
                Ok(processed) => {
                    images.push(ImageContent {
                        data: processed.data,
                        mime_type: processed.mime_type,
                    });
                    if processed.hints.is_empty() {
                        text.push_str(&format!(
                            "<file name=\"{}\"></file>\n",
                            absolute_path.display()
                        ));
                    } else {
                        text.push_str(&format!(
                            "<file name=\"{}\">{}</file>\n",
                            absolute_path.display(),
                            processed.hints.join("\n")
                        ));
                    }
                }
            }
        } else {
            // Handle text file. Node's utf-8 read is lossy (U+FFFD), like
            // `String::from_utf8_lossy`.
            let bytes = tokio::fs::read(&absolute_path).await.map_err(|e| {
                FileProcessError::ReadFailed {
                    path: absolute_path.clone(),
                    message: e.to_string(),
                }
            })?;
            let content = String::from_utf8_lossy(&bytes);
            text.push_str(&format!(
                "<file name=\"{}\">\n{content}\n</file>\n",
                absolute_path.display()
            ));
        }
    }

    Ok(ProcessedFiles { text, images })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "rpi-file-processor-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn test_text_file_wrapped_in_file_tag() {
        let temp = TempDir::new();
        let file = temp.0.join("note.txt");
        std::fs::write(&file, "hello world").expect("write");

        let result = process_file_arguments(&["note.txt".to_owned()], &temp.0, true)
            .await
            .expect("process");
        assert_eq!(
            result.text,
            format!("<file name=\"{}\">\nhello world\n</file>\n", file.display())
        );
        assert!(result.images.is_empty());
    }

    #[test]
    fn test_missing_file_is_error() {
        let temp = TempDir::new();
        let result =
            tokio::runtime::Runtime::new()
                .expect("runtime")
                .block_on(process_file_arguments(
                    &["nope.txt".to_owned()],
                    &temp.0,
                    true,
                ));
        match result {
            Err(FileProcessError::FileNotFound(path)) => {
                assert!(path.ends_with("nope.txt"));
            }
            other => panic!("expected FileNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_empty_file_is_skipped() {
        let temp = TempDir::new();
        std::fs::write(temp.0.join("empty.txt"), "").expect("write");

        let result = process_file_arguments(&["empty.txt".to_owned()], &temp.0, true)
            .await
            .expect("process");
        assert_eq!(result.text, "");
        assert!(result.images.is_empty());
    }

    #[tokio::test]
    async fn test_png_file_becomes_image_attachment() {
        let temp = TempDir::new();
        // 1x1 red PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d,
            0xb0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        std::fs::write(temp.0.join("img.png"), png).expect("write");

        let result = process_file_arguments(&["img.png".to_owned()], &temp.0, true)
            .await
            .expect("process");
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].mime_type, "image/png");
        assert!(!result.images[0].data.is_empty());
        assert!(result.text.starts_with(&format!(
            "<file name=\"{}\">",
            temp.0.join("img.png").display()
        )));
        assert!(result.text.ends_with("</file>\n"));
    }
}
