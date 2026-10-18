//! Photos in chat (docs/decisions/media-messages.md §1): what the sender makes of a picture, and what
//! the receiver accepts as one.
//!
//! The sender re-encodes: decode under limits, turn by the camera's orientation, cap the long side,
//! JPEG — the metadata (a phone's location among it) does not survive. A small preview rides in the
//! offer. The receiver trusts none of it: a claim over the limits, a preview that is not the small
//! JPEG it says, a downloaded file whose header disagrees — each turns the photo back into a plain
//! file, which the chat shows as a plate and never decodes.

use std::io::Cursor;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use omni_types::{AttachmentPresentationKindV1, AttachmentPresentationV1, OmniError, OmniResult};

/// The long side of the photo that is sent.
pub const PHOTO_SENT_MAX_SIDE: u32 = 2560;
/// The long side of the preview inside the offer.
pub const PHOTO_PREVIEW_MAX_SIDE: u32 = 320;
const PHOTO_SENT_JPEG_QUALITY: u8 = 82;
const PHOTO_PREVIEW_JPEG_QUALITY: u8 = 60;

/// What a sender may pick to send as a photo: bigger is sent as a file.
pub const PHOTO_SOURCE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Decoder limits for a picture the user picked: 50 Mpx, and no more than this much memory.
const PHOTO_SOURCE_MAX_PIXELS: u64 = 50_000_000;
const PHOTO_DECODE_MAX_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

/// What a receiver accepts as a photo. Over any of these it is a plain file.
pub const PHOTO_RECEIVED_MAX_SIDE: u32 = 4096;
pub const PHOTO_RECEIVED_MAX_BYTES: i64 = 12 * 1024 * 1024;
pub const PHOTO_PREVIEW_MAX_BYTES: usize = 16 * 1024;

/// A picture made ready to send as a photo.
pub struct PreparedPhoto {
    pub jpeg: Vec<u8>,
    pub presentation: AttachmentPresentationV1,
}

fn decode_limits(max_alloc: u64) -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(65_535);
    limits.max_image_height = Some(65_535);
    limits.max_alloc = Some(max_alloc);
    limits
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> OmniResult<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(&image.to_rgb8())
        .map_err(|error| OmniError::Storage(format!("failed to encode a photo as JPEG: {error}")))?;
    Ok(bytes)
}

fn fit_within(image: DynamicImage, max_side: u32) -> DynamicImage {
    if image.width() <= max_side && image.height() <= max_side {
        image
    } else {
        image.resize(max_side, max_side, FilterType::Lanczos3)
    }
}

/// Decode a picture the user picked, under limits: the pixel count is checked from the header
/// BEFORE the pixels are decoded, so a small file claiming a huge picture costs nothing.
pub(crate) fn decode_picked_picture(source_path: &Path, max_bytes: u64, max_pixels: u64) -> OmniResult<DynamicImage> {
    let size = std::fs::metadata(source_path)
        .map_err(|error| OmniError::Storage(format!("failed to inspect '{}': {error}", source_path.display())))?
        .len();
    if size > max_bytes {
        return Err(OmniError::InvalidInput(format!("the picture exceeds the {max_bytes} byte limit")));
    }
    let mut reader = ImageReader::open(source_path)
        .and_then(ImageReader::with_guessed_format)
        .map_err(|error| OmniError::InvalidInput(format!("failed to open the picture: {error}")))?;
    reader.limits(decode_limits(PHOTO_DECODE_MAX_ALLOC_BYTES));
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| OmniError::InvalidInput(format!("not a picture this app reads: {error}")))?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > max_pixels {
        return Err(OmniError::InvalidInput("the picture's dimensions are too large".to_string()));
    }
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)
        .map_err(|error| OmniError::InvalidInput(format!("failed to decode the picture: {error}")))?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Ok(image)
}

/// Re-encode a picked picture as the photo to send (and its preview).
pub fn prepare_photo(source_path: &Path) -> OmniResult<PreparedPhoto> {
    let image = decode_picked_picture(source_path, PHOTO_SOURCE_MAX_BYTES, PHOTO_SOURCE_MAX_PIXELS)?;
    let sent = fit_within(image, PHOTO_SENT_MAX_SIDE);
    let jpeg = encode_jpeg(&sent, PHOTO_SENT_JPEG_QUALITY)?;
    let mut preview = encode_jpeg(&fit_within(sent.clone(), PHOTO_PREVIEW_MAX_SIDE), PHOTO_PREVIEW_JPEG_QUALITY)?;
    if preview.len() > PHOTO_PREVIEW_MAX_BYTES {
        // A busy picture: a smaller, rougher preview rather than none.
        preview = encode_jpeg(&fit_within(sent.clone(), PHOTO_PREVIEW_MAX_SIDE / 2), 45)?;
    }
    Ok(PreparedPhoto {
        presentation: AttachmentPresentationV1 {
            kind: AttachmentPresentationKindV1::Photo,
            width: sent.width(),
            height: sent.height(),
            preview_jpeg_b64: (preview.len() <= PHOTO_PREVIEW_MAX_BYTES).then(|| B64.encode(&preview)),
        },
        jpeg,
    })
}

/// A JPEG's dimensions from its header only (nothing decoded); `None` when it is not a JPEG.
fn jpeg_dimensions(reader: ImageReader<impl std::io::BufRead + std::io::Seek>) -> Option<(u32, u32)> {
    let reader = reader.with_guessed_format().ok()?;
    if reader.format() != Some(ImageFormat::Jpeg) {
        return None;
    }
    reader.into_dimensions().ok()
}

/// What a received offer may be shown as. `None` = a plain file (the offer itself still stands).
pub fn accept_received_photo_presentation(
    presentation: Option<&AttachmentPresentationV1>,
    file_size_bytes: i64,
) -> Option<AttachmentPresentationV1> {
    let presentation = presentation?;
    match presentation.kind {
        AttachmentPresentationKindV1::Photo => {}
    }
    let sides_ok = |w: u32, h: u32, max: u32| w > 0 && h > 0 && w <= max && h <= max;
    if !sides_ok(presentation.width, presentation.height, PHOTO_RECEIVED_MAX_SIDE)
        || file_size_bytes <= 0
        || file_size_bytes > PHOTO_RECEIVED_MAX_BYTES
    {
        return None;
    }
    // The preview must be the small JPEG it claims to be; a bad one is dropped, the photo stays.
    let preview_jpeg_b64 = presentation.preview_jpeg_b64.as_deref().and_then(|b64| {
        if b64.len() > PHOTO_PREVIEW_MAX_BYTES * 4 / 3 + 4 {
            return None;
        }
        let bytes = B64.decode(b64).ok()?;
        let (w, h) = jpeg_dimensions(ImageReader::new(Cursor::new(&bytes)))?;
        sides_ok(w, h, PHOTO_PREVIEW_MAX_SIDE).then(|| b64.to_string())
    });
    Some(AttachmentPresentationV1 {
        kind: AttachmentPresentationKindV1::Photo,
        width: presentation.width,
        height: presentation.height,
        preview_jpeg_b64,
    })
}

/// Whether a downloaded photo (its plaintext bytes — the file may be encrypted at rest) is what its
/// offer said: a JPEG of the stated size, within the limit.
pub fn downloaded_photo_matches(bytes: &[u8], presentation: &AttachmentPresentationV1) -> bool {
    let reader = ImageReader::new(Cursor::new(bytes));
    jpeg_dimensions(reader) == Some((presentation.width, presentation.height))
        && presentation.width <= PHOTO_RECEIVED_MAX_SIDE
        && presentation.height <= PHOTO_RECEIVED_MAX_SIDE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_png(name: &str, width: u32, height: u32) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("omni-photo-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.png", std::process::id()));
        image::RgbImage::from_fn(width, height, |x, y| image::Rgb([(x * 7 % 256) as u8, (y * 3 % 256) as u8, 40]))
            .save(&path)
            .unwrap();
        path
    }

    #[test]
    fn a_photo_is_capped_and_its_preview_is_the_small_jpeg_it_says() {
        let source = temp_png("tall", 1200, 4000);
        let prepared = prepare_photo(&source).unwrap();
        assert_eq!((prepared.presentation.width, prepared.presentation.height), (768, 2560));
        assert_eq!(image::load_from_memory(&prepared.jpeg).unwrap().height(), 2560);
        let accepted = accept_received_photo_presentation(Some(&prepared.presentation), prepared.jpeg.len() as i64)
            .expect("an honest photo is accepted");
        assert!(accepted.preview_jpeg_b64.is_some());
    }

    #[test]
    fn a_receiver_turns_a_lying_photo_into_a_file() {
        let honest = AttachmentPresentationV1 {
            kind: AttachmentPresentationKindV1::Photo,
            width: 1000,
            height: 800,
            preview_jpeg_b64: None,
        };
        // Over the side limit, or a file too big for a photo: a plain file.
        let huge = AttachmentPresentationV1 { width: PHOTO_RECEIVED_MAX_SIDE + 1, ..honest.clone() };
        assert!(accept_received_photo_presentation(Some(&huge), 1000).is_none());
        assert!(accept_received_photo_presentation(Some(&honest), PHOTO_RECEIVED_MAX_BYTES + 1).is_none());
        // A preview that is not a small JPEG is dropped; the photo stays.
        let png_preview = std::fs::read(temp_png("preview", 64, 64)).unwrap();
        let bad_preview = AttachmentPresentationV1 { preview_jpeg_b64: Some(B64.encode(png_preview)), ..honest.clone() };
        let accepted = accept_received_photo_presentation(Some(&bad_preview), 1000).unwrap();
        assert!(accepted.preview_jpeg_b64.is_none());
        // A downloaded file whose header disagrees with the offer is not shown as a photo.
        let prepared = prepare_photo(&temp_png("real", 1000, 800)).unwrap();
        let downloaded = std::env::temp_dir().join("omni-photo-tests").join(format!("dl-{}.jpg", std::process::id()));
        std::fs::write(&downloaded, &prepared.jpeg).unwrap();
        assert!(downloaded_photo_matches(&std::fs::read(&downloaded).unwrap(), &honest));
        let claimed_bigger = AttachmentPresentationV1 { width: 2000, ..honest };
        assert!(!downloaded_photo_matches(&std::fs::read(&downloaded).unwrap(), &claimed_bigger));
    }

    #[test]
    fn a_picture_claiming_too_many_pixels_is_refused_before_it_is_decoded() {
        let source = temp_png("wide", 9000, 9000);
        let error = decode_picked_picture(&source, PHOTO_SOURCE_MAX_BYTES, 1_000_000).err().unwrap();
        assert!(error.to_string().contains("too large"), "{error}");
    }
}
