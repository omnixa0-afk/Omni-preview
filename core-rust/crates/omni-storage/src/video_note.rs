//! Round video messages: a short MP4 the chat draws as a circle. The sender's shell records it; the first
//! frame's small JPEG travels inside the file as a box of our own, which players skip, so a forwarded
//! one keeps it.
//!
//! The receiver trusts none of it. Only the boxes are read here, never the media: the length and the size
//! come from the file's own headers, and one that disagrees with its offer, or is no such MP4, is a plain
//! file and is never handed to a decoder.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use omni_types::{AttachmentPresentationKindV1, AttachmentPresentationV1, OmniError, OmniResult};

/// The longest recording.
pub const VIDEO_NOTE_MAX_MS: u32 = 60 * 1000;
/// A recording stops a little after the limit it was told; the offer's length may be this far off.
const VIDEO_NOTE_LENGTH_SLACK_MS: u32 = 5_000;
/// What a receiver accepts as a video message: well over what a minute at our bitrate takes.
pub const VIDEO_NOTE_RECEIVED_MAX_BYTES: i64 = 32 * 1024 * 1024;
const VIDEO_NOTE_MAX_SIDE: u32 = 1920;

const PREVIEW_BOX: &[u8; 4] = b"omnp";
const BOX_HEADER: usize = 8;

/// A video message as its file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoNoteFile {
    pub duration_ms: u32,
    pub width: u32,
    pub height: u32,
    pub preview_jpeg: Option<Vec<u8>>,
}

impl VideoNoteFile {
    pub fn presentation(&self) -> AttachmentPresentationV1 {
        AttachmentPresentationV1 {
            kind: AttachmentPresentationKindV1::VideoNote,
            width: self.width,
            height: self.height,
            preview_jpeg_b64: self.preview_jpeg.as_ref().map(|jpeg| B64.encode(jpeg)),
            duration_ms: Some(self.duration_ms),
            waveform_b64: None,
        }
    }
}

fn not_video_note(why: &str) -> OmniError {
    OmniError::InvalidInput(format!("not a video message: {why}"))
}

/// The boxes directly inside `bytes`: (type, content).
fn boxes(bytes: &[u8]) -> impl Iterator<Item = OmniResult<(&[u8], &[u8])>> {
    let mut at = 0usize;
    std::iter::from_fn(move || {
        if at >= bytes.len() {
            return None;
        }
        let parsed = (|| {
            let cut = || not_video_note("a cut box");
            let head = bytes.get(at..at + BOX_HEADER).ok_or_else(cut)?;
            let kind = &head[4..8];
            let short = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
            let (size, header) = match short {
                0 => ((bytes.len() - at) as u64, BOX_HEADER),
                1 => {
                    let wide: [u8; 8] = bytes
                        .get(at + BOX_HEADER..at + BOX_HEADER + 8)
                        .ok_or_else(cut)?
                        .try_into()
                        .map_err(|_| cut())?;
                    (u64::from_be_bytes(wide), BOX_HEADER + 8)
                }
                size => (size, BOX_HEADER),
            };
            let size = usize::try_from(size).map_err(|_| cut())?;
            if size < header {
                return Err(cut());
            }
            let end = at.checked_add(size).ok_or_else(cut)?;
            let content = bytes.get(at + header..end).ok_or_else(cut)?;
            at = end;
            Ok((kind, content))
        })();
        if parsed.is_err() {
            at = bytes.len();
        }
        Some(parsed)
    })
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The movie's length in ms, from `mvhd`.
fn movie_ms(mvhd: &[u8]) -> Option<u64> {
    let (timescale, duration) = match *mvhd.first()? {
        0 => (be_u32(mvhd, 12)?, u64::from(be_u32(mvhd, 16)?)),
        1 => (be_u32(mvhd, 20)?, be_u64(mvhd, 24)?),
        _ => return None,
    };
    (timescale > 0).then(|| u128::from(duration) * 1000 / u128::from(timescale)).and_then(|ms| u64::try_from(ms).ok())
}

/// A track's picture size, from `tkhd`; (0, 0) for a sound track.
fn track_size(tkhd: &[u8]) -> Option<(u32, u32)> {
    let at = match *tkhd.first()? {
        0 => 76,
        1 => 88,
        _ => return None,
    };
    Some((be_u32(tkhd, at)? >> 16, be_u32(tkhd, at + 4)? >> 16))
}

/// A video message's file, checked by its boxes.
pub fn read_video_note(bytes: &[u8]) -> OmniResult<VideoNoteFile> {
    if bytes.len() as i64 > VIDEO_NOTE_RECEIVED_MAX_BYTES {
        return Err(not_video_note("too big"));
    }
    let mut first = true;
    let mut duration_ms = None;
    let mut size = None;
    let mut preview_jpeg = None;
    for found in boxes(bytes) {
        let (kind, content) = found?;
        if first && kind != b"ftyp" {
            return Err(not_video_note("no MP4 header"));
        }
        first = false;
        if kind == PREVIEW_BOX {
            preview_jpeg = Some(content.to_vec());
        }
        if kind != b"moov" {
            continue;
        }
        for inner in boxes(content) {
            let (inner_kind, inner_content) = inner?;
            if inner_kind == b"mvhd" {
                duration_ms = movie_ms(inner_content);
            }
            if inner_kind != b"trak" {
                continue;
            }
            for part in boxes(inner_content) {
                let (part_kind, part_content) = part?;
                if part_kind == b"tkhd" {
                    size = track_size(part_content).filter(|(w, h)| *w > 0 && *h > 0).or(size);
                }
            }
        }
    }
    let duration_ms = duration_ms.filter(|ms| *ms > 0).ok_or_else(|| not_video_note("no length"))?;
    if duration_ms > u64::from(VIDEO_NOTE_MAX_MS + VIDEO_NOTE_LENGTH_SLACK_MS) {
        return Err(not_video_note("too long"));
    }
    let (width, height) = size.ok_or_else(|| not_video_note("no picture"))?;
    if width > VIDEO_NOTE_MAX_SIDE || height > VIDEO_NOTE_MAX_SIDE {
        return Err(not_video_note("too large a picture"));
    }
    Ok(VideoNoteFile { duration_ms: duration_ms as u32, width, height, preview_jpeg })
}

/// The recording with its first frame's small JPEG added as a box of our own.
pub fn with_preview_box(mp4: &[u8], jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(mp4.len() + jpeg.len() + BOX_HEADER);
    out.extend_from_slice(mp4);
    out.extend_from_slice(&((jpeg.len() + BOX_HEADER) as u32).to_be_bytes());
    out.extend_from_slice(PREVIEW_BOX);
    out.extend_from_slice(jpeg);
    out
}

/// What a received offer may be shown as. `None` = a plain file (the offer itself still stands).
pub fn accept_received_video_note_presentation(
    presentation: &AttachmentPresentationV1,
    file_size_bytes: i64,
) -> Option<AttachmentPresentationV1> {
    let duration_ms = presentation.duration_ms?;
    let side_ok = |side: u32| side > 0 && side <= VIDEO_NOTE_MAX_SIDE;
    if duration_ms == 0
        || duration_ms > VIDEO_NOTE_MAX_MS + VIDEO_NOTE_LENGTH_SLACK_MS
        || !side_ok(presentation.width)
        || !side_ok(presentation.height)
        || file_size_bytes <= 0
        || file_size_bytes > VIDEO_NOTE_RECEIVED_MAX_BYTES
    {
        return None;
    }
    Some(AttachmentPresentationV1 {
        kind: AttachmentPresentationKindV1::VideoNote,
        width: presentation.width,
        height: presentation.height,
        // A bad preview is dropped, the message stays: it draws an empty circle.
        preview_jpeg_b64: crate::photo::accepted_preview_b64(presentation.preview_jpeg_b64.as_deref()),
        duration_ms: Some(duration_ms),
        waveform_b64: None,
    })
}

/// Whether a downloaded video message (its plaintext bytes) is what its offer said.
pub fn downloaded_video_note_matches(bytes: &[u8], presentation: &AttachmentPresentationV1) -> bool {
    let Some(claimed) = presentation.duration_ms else {
        return false;
    };
    read_video_note(bytes).is_ok_and(|file| {
        file.duration_ms.abs_diff(claimed) <= VIDEO_NOTE_LENGTH_SLACK_MS
            && (file.width, file.height) == (presentation.width, presentation.height)
    })
}

/// A minimal MP4: enough boxes for the checks above. For tests here and in the app's.
pub fn test_mp4(duration_ms: u32, width: u32, height: u32) -> Vec<u8> {
    fn boxed(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut out = ((content.len() + BOX_HEADER) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(content);
        out
    }
    let mut mvhd = vec![0u8; 100];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&duration_ms.to_be_bytes());
    let mut tkhd = vec![0u8; 84];
    tkhd[76..80].copy_from_slice(&(width << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&(height << 16).to_be_bytes());
    let trak = boxed(b"trak", &boxed(b"tkhd", &tkhd));
    let moov = boxed(b"moov", &[boxed(b"mvhd", &mvhd), trak].concat());
    [boxed(b"ftyp", b"mp42\0\0\0\0mp42"), boxed(b"mdat", &[7u8; 64]), moov].concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_says_its_length_and_size_and_keeps_its_preview() {
        let file = with_preview_box(&test_mp4(4_200, 854, 480), b"jpeg bytes");
        let read = read_video_note(&file).unwrap();
        assert_eq!((read.duration_ms, read.width, read.height), (4_200, 854, 480));
        assert_eq!(read.preview_jpeg.as_deref(), Some(&b"jpeg bytes"[..]));
    }

    #[test]
    fn what_is_no_such_mp4_is_no_video_message() {
        assert!(read_video_note(b"RIFF....WAVE").is_err());
        assert!(read_video_note(&[]).is_err());
        let good = test_mp4(1_000, 640, 480);
        assert!(read_video_note(&good[..good.len() - 5]).is_err(), "cut");
        assert!(read_video_note(&good[20..]).is_err(), "no header");
        assert!(read_video_note(&test_mp4(0, 640, 480)).is_err(), "no length");
        assert!(read_video_note(&test_mp4(1_000, 0, 0)).is_err(), "no picture");
        assert!(read_video_note(&test_mp4(600_000, 640, 480)).is_err(), "too long");
        assert!(read_video_note(&test_mp4(1_000, 8_000, 480)).is_err(), "too large");
    }

    #[test]
    fn a_box_claiming_more_than_there_is_stops_the_reading() {
        let mut bad = test_mp4(1_000, 640, 480);
        bad[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(read_video_note(&bad).is_err());
    }

    #[test]
    fn the_file_must_match_its_offer() {
        let file = test_mp4(10_000, 640, 480);
        let honest = read_video_note(&file).unwrap().presentation();
        assert!(downloaded_video_note_matches(&file, &honest));
        let longer = AttachmentPresentationV1 { duration_ms: Some(40_000), ..honest.clone() };
        assert!(!downloaded_video_note_matches(&file, &longer));
        let wider = AttachmentPresentationV1 { width: 700, ..honest };
        assert!(!downloaded_video_note_matches(&file, &wider));
    }

    #[test]
    fn a_claim_over_the_limits_is_a_plain_file() {
        let good = read_video_note(&test_mp4(10_000, 640, 480)).unwrap().presentation();
        assert!(accept_received_video_note_presentation(&good, 1_000).is_some());
        assert!(accept_received_video_note_presentation(&good, VIDEO_NOTE_RECEIVED_MAX_BYTES + 1).is_none());
        let endless = AttachmentPresentationV1 { duration_ms: Some(VIDEO_NOTE_MAX_MS * 2), ..good.clone() };
        assert!(accept_received_video_note_presentation(&endless, 1_000).is_none());
        let huge = AttachmentPresentationV1 { height: VIDEO_NOTE_MAX_SIDE + 1, ..good.clone() };
        assert!(accept_received_video_note_presentation(&huge, 1_000).is_none());
        let bad_preview = AttachmentPresentationV1 { preview_jpeg_b64: Some(B64.encode(b"no jpeg")), ..good };
        let kept = accept_received_video_note_presentation(&bad_preview, 1_000).expect("the message stays");
        assert!(kept.preview_jpeg_b64.is_none());
    }
}
