use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const IMAGE_MAX_BYTES: u64 = 20 * 1024 * 1024;
const COPY_PREFIX: &str = "image-";
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG_SIGNATURE: [u8; 3] = [0xff, 0xd8, 0xff];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn media_type(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub given: String,
    pub kind: ImageKind,
    pub bytes: Vec<u8>,
}

fn detect(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        Some(ImageKind::Png)
    } else if bytes.starts_with(&JPEG_SIGNATURE) {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

pub fn read_images(paths: &[PathBuf]) -> Result<Vec<Image>, String> {
    paths.iter().map(|path| read_image(path)).collect()
}

fn read_image(path: &Path) -> Result<Image, String> {
    let read_error =
        |error: std::io::Error| format!("cannot read image {}: {error}", path.display());
    let mut file = std::fs::File::open(path).map_err(read_error)?;
    let size = file.metadata().map_err(read_error)?.len();
    if size > IMAGE_MAX_BYTES {
        return Err(format!(
            "image {} is {size} bytes, more than the limit of {IMAGE_MAX_BYTES} bytes",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(read_error)?;
    let kind = detect(&bytes).ok_or_else(|| {
        format!(
            "image {} is not a png, jpeg, gif or webp file",
            path.display()
        )
    })?;
    Ok(Image {
        given: path.to_string_lossy().into_owned(),
        kind,
        bytes,
    })
}

fn copy_path(tempdir: &Path, index: usize, kind: ImageKind) -> PathBuf {
    tempdir.join(format!("{COPY_PREFIX}{}.{}", index + 1, kind.extension()))
}

pub fn write_copies(
    images: &[Image],
    tempdir: &Path,
    dry_run: bool,
) -> Result<Vec<PathBuf>, String> {
    let mut copies = Vec::new();
    for (index, image) in images.iter().enumerate() {
        let path = copy_path(tempdir, index, image.kind);
        if !dry_run {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .and_then(|mut file| file.write_all(&image.bytes))
                .map_err(|error| {
                    format!(
                        "cannot copy image {} to {}: {error}",
                        image.given,
                        path.display()
                    )
                })?;
        }
        copies.push(path);
    }
    Ok(copies)
}

pub fn base64(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let bits = u32::from_be_bytes([0, triple[0], triple[1], triple[2]]);
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (bits >> (18 - 6 * position)) & 0x3f;
                encoded.push(BASE64_ALPHABET[index as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd, 0x00]), "//79AA==");
    }

    #[test]
    fn detect_reads_the_file_signature() {
        assert_eq!(detect(&PNG_SIGNATURE), Some(ImageKind::Png));
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageKind::Jpeg));
        assert_eq!(detect(b"GIF89a...."), Some(ImageKind::Gif));
        assert_eq!(detect(b"GIF87a...."), Some(ImageKind::Gif));
        assert_eq!(detect(b"RIFF\x10\0\0\0WEBPVP8 "), Some(ImageKind::Webp));
        assert_eq!(detect(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(detect(b"BM\x10\0\0\0"), None);
        assert_eq!(detect(b"%PDF-1.7"), None);
        assert_eq!(detect(b""), None);
    }

    #[test]
    fn read_images_rejects_missing_unknown_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.png");
        let error = read_images(std::slice::from_ref(&missing)).unwrap_err();
        assert!(
            error.starts_with(&format!("cannot read image {}: ", missing.display())),
            "{error}"
        );

        let text = dir.path().join("notes.png");
        std::fs::write(&text, "not an image").unwrap();
        assert_eq!(
            read_images(std::slice::from_ref(&text)).unwrap_err(),
            format!(
                "image {} is not a png, jpeg, gif or webp file",
                text.display()
            )
        );

        let large = dir.path().join("large.png");
        let file = std::fs::File::create(&large).unwrap();
        file.set_len(IMAGE_MAX_BYTES + 1).unwrap();
        assert_eq!(
            read_images(std::slice::from_ref(&large)).unwrap_err(),
            format!(
                "image {} is {} bytes, more than the limit of {IMAGE_MAX_BYTES} bytes",
                large.display(),
                IMAGE_MAX_BYTES + 1
            )
        );

        let error = read_images(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(error.starts_with("cannot read image "), "{error}");
    }

    #[test]
    fn write_copies_keeps_the_order_and_the_type() {
        let dir = tempfile::tempdir().unwrap();
        let images = vec![
            Image {
                given: "shots/a.png".to_string(),
                kind: ImageKind::Png,
                bytes: PNG_SIGNATURE.to_vec(),
            },
            Image {
                given: "b".to_string(),
                kind: ImageKind::Jpeg,
                bytes: JPEG_SIGNATURE.to_vec(),
            },
        ];
        let planned = write_copies(&images, dir.path(), true).unwrap();
        assert_eq!(
            planned,
            [
                dir.path().join("image-1.png"),
                dir.path().join("image-2.jpg")
            ]
        );
        assert!(!planned[0].exists());
        let copies = write_copies(&images, dir.path(), false).unwrap();
        assert_eq!(copies, planned);
        assert_eq!(std::fs::read(&copies[1]).unwrap(), JPEG_SIGNATURE);
    }
}
