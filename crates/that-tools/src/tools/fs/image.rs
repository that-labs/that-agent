//! Image read with validation and auto-resize for vision-capable LLMs.

use super::FsError;
use crate::tools::path_guard;
use serde::{Deserialize, Serialize};
use std::path::Path;

const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024; // 5 MB
const MAX_LONG_SIDE: u32 = 1568;
const ALLOWED_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Metadata + pixel data returned by `image_read`.
#[derive(Debug, Clone)]
pub struct ImageReadResult {
    pub path: String,
    pub original_width: u32,
    pub original_height: u32,
    pub resized: bool,
    pub final_width: u32,
    pub final_height: u32,
    pub size_bytes: usize,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// JSON-safe metadata (no binary data).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageReadMeta {
    pub path: String,
    pub original_width: u32,
    pub original_height: u32,
    pub resized: bool,
    pub final_width: u32,
    pub final_height: u32,
    pub size_bytes: usize,
    pub mime_type: String,
}

impl From<&ImageReadResult> for ImageReadMeta {
    fn from(r: &ImageReadResult) -> Self {
        Self {
            path: r.path.clone(),
            original_width: r.original_width,
            original_height: r.original_height,
            resized: r.resized,
            final_width: r.final_width,
            final_height: r.final_height,
            size_bytes: r.size_bytes,
            mime_type: r.mime_type.clone(),
        }
    }
}

fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Read an image file, validate, and optionally resize for LLM vision.
pub fn image_read(path: &Path) -> Result<ImageReadResult, FsError> {
    if !path.exists() {
        return Err(FsError::NotFound(path.to_path_buf()));
    }
    let path = &path_guard::guard(path)?;

    // Extension check
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return Err(FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "unsupported image format '.{ext}'. Supported: {}",
                ALLOWED_EXTENSIONS.join(", ")
            ),
        )));
    }

    // Size check
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "image too large: {} bytes (max {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ),
        )));
    }

    let raw_bytes = std::fs::read(path)?;
    let img = image::load_from_memory(&raw_bytes).map_err(|e| {
        FsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to decode image: {e}"),
        ))
    })?;

    let (orig_w, orig_h) = (img.width(), img.height());
    let long_side = orig_w.max(orig_h);

    if long_side > MAX_LONG_SIDE {
        // Resize proportionally
        let scale = MAX_LONG_SIDE as f64 / long_side as f64;
        let new_w = (orig_w as f64 * scale).round() as u32;
        let new_h = (orig_h as f64 * scale).round() as u32;
        let resized = img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3);
        let mut buf = std::io::Cursor::new(Vec::new());
        resized
            .write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| {
                FsError::Io(std::io::Error::other(format!(
                    "failed to encode resized image: {e}"
                )))
            })?;
        let data = buf.into_inner();
        Ok(ImageReadResult {
            path: path.to_string_lossy().to_string(),
            original_width: orig_w,
            original_height: orig_h,
            resized: true,
            final_width: new_w,
            final_height: new_h,
            size_bytes: data.len(),
            mime_type: "image/png".to_string(),
            data,
        })
    } else {
        // Use original bytes as-is
        let mime = mime_for_ext(&ext).to_string();
        Ok(ImageReadResult {
            path: path.to_string_lossy().to_string(),
            original_width: orig_w,
            original_height: orig_h,
            resized: false,
            final_width: orig_w,
            final_height: orig_h,
            size_bytes: raw_bytes.len(),
            mime_type: mime,
            data: raw_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_test_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn test_valid_png() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.png");
        fs::write(&path, make_test_png(100, 80)).unwrap();

        let result = image_read(&path).unwrap();
        assert_eq!(result.original_width, 100);
        assert_eq!(result.original_height, 80);
        assert!(!result.resized);
        assert_eq!(result.mime_type, "image/png");
        assert!(!result.data.is_empty());
    }

    #[test]
    fn test_oversized_image_gets_resized() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.png");
        fs::write(&path, make_test_png(3000, 2000)).unwrap();

        let result = image_read(&path).unwrap();
        assert!(result.resized);
        assert_eq!(result.original_width, 3000);
        assert_eq!(result.original_height, 2000);
        assert!(result.final_width <= MAX_LONG_SIDE);
        assert!(result.final_height <= MAX_LONG_SIDE);
        assert_eq!(result.mime_type, "image/png");
    }

    #[test]
    fn test_non_image_extension_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.txt");
        fs::write(&path, "not an image").unwrap();

        let err = image_read(&path).unwrap_err();
        assert!(err.to_string().contains("unsupported image format"));
    }

    #[test]
    fn test_file_too_large() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("huge.png");
        // Write > 5MB of data with a .png extension
        let data = vec![0u8; (MAX_FILE_SIZE + 1) as usize];
        fs::write(&path, data).unwrap();

        let err = image_read(&path).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn test_not_found() {
        let result = image_read(Path::new("/nonexistent/image.png"));
        assert!(matches!(result.unwrap_err(), FsError::NotFound(_)));
    }

    #[test]
    fn test_valid_jpeg() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.jpg");
        // Create a valid JPEG
        let img = image::RgbImage::from_fn(50, 50, |_, _| image::Rgb([255, 0, 0]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
        fs::write(&path, buf.into_inner()).unwrap();

        let result = image_read(&path).unwrap();
        assert_eq!(result.original_width, 50);
        assert_eq!(result.mime_type, "image/jpeg");
        assert!(!result.resized);
    }
}
