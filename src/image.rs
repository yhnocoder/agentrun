use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const IMAGE_MAX_BYTES: u64 = 20 * 1024 * 1024;
const COPY_PREFIX: &str = "image-";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const JPEG_SIGNATURE: [u8; 3] = [0xff, 0xd8, 0xff];
const JPEG_LS_MARKER: u8 = 0xf7;
const PNG_HEADER_LENGTH: u32 = 13;
const PNG_ANIMATION_CHUNK: &[u8; 4] = b"acTL";
const PNG_DATA_CHUNK: &[u8; 4] = b"IDAT";

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
        still_png(bytes).then_some(ImageKind::Png)
    } else if bytes.starts_with(&JPEG_SIGNATURE) {
        (bytes.get(3) != Some(&JPEG_LS_MARKER)).then_some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

fn still_png(bytes: &[u8]) -> bool {
    let chunk_length = |offset: usize| {
        bytes
            .get(offset..offset + 4)
            .map(|field| u32::from_be_bytes(field.try_into().expect("four bytes")))
    };
    if chunk_length(PNG_SIGNATURE.len()) != Some(PNG_HEADER_LENGTH)
        || bytes.get(12..16) != Some(b"IHDR")
    {
        return false;
    }
    let mut offset = PNG_SIGNATURE.len();
    while let (Some(length), Some(kind)) = (chunk_length(offset), bytes.get(offset + 4..offset + 8))
    {
        if kind == PNG_ANIMATION_CHUNK {
            return false;
        }
        if kind == PNG_DATA_CHUNK {
            return true;
        }
        offset += 12 + length as usize;
    }
    true
}

pub fn read_images(paths: &[PathBuf]) -> Result<Vec<Image>, String> {
    paths.iter().map(|path| read_image(path)).collect()
}

fn read_image(path: &Path) -> Result<Image, String> {
    let read_error =
        |error: std::io::Error| format!("cannot read image {}: {error}", path.display());
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(read_error)?;
    if !file.metadata().map_err(read_error)?.is_file() {
        return Err(format!("image {} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(read_error)?;
    if bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(format!(
            "image {} is larger than the limit of {IMAGE_MAX_BYTES} bytes",
            path.display()
        ));
    }
    let kind = detect(&bytes).ok_or_else(|| {
        format!(
            "image {} is not a png, jpeg, gif or webp file (animated png and jpeg-ls are not supported)",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn png(chunks: &[&[u8; 4]]) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        for (index, kind) in chunks.iter().enumerate() {
            let length: u32 = if index == 0 { 13 } else { 2 };
            bytes.extend(length.to_be_bytes());
            bytes.extend(*kind);
            bytes.extend(vec![0; length as usize + 4]);
        }
        bytes
    }

    #[test]
    fn detect_reads_the_file_signature() {
        assert_eq!(detect(&png(&[b"IHDR", b"IDAT"])), Some(ImageKind::Png));
        assert_eq!(
            detect(&png(&[b"IHDR", b"tEXt", b"IDAT", b"acTL"])),
            Some(ImageKind::Png)
        );
        assert_eq!(detect(&png(&[b"IHDR"])), Some(ImageKind::Png));
        assert_eq!(detect(&png(&[b"IHDR", b"acTL", b"IDAT"])), None);
        assert_eq!(detect(&png(&[b"tEXt", b"IDAT"])), None);
        assert_eq!(detect(&PNG_SIGNATURE), None);
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xe0]), Some(ImageKind::Jpeg));
        assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xf7]), None);
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
                "image {} is not a png, jpeg, gif or webp file (animated png and jpeg-ls are not supported)",
                text.display()
            )
        );

        let large = dir.path().join("large.png");
        let file = std::fs::File::create(&large).unwrap();
        file.set_len(IMAGE_MAX_BYTES + 1).unwrap();
        assert_eq!(
            read_images(std::slice::from_ref(&large)).unwrap_err(),
            format!(
                "image {} is larger than the limit of {IMAGE_MAX_BYTES} bytes",
                large.display()
            )
        );

        let fifo = dir.path().join("fifo.png");
        let fifo_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        for path in [dir.path().to_path_buf(), fifo, PathBuf::from("/dev/zero")] {
            assert_eq!(
                read_images(std::slice::from_ref(&path)).unwrap_err(),
                format!("image {} is not a regular file", path.display())
            );
        }
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
