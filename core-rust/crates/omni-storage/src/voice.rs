//! Voice messages (docs/decisions/media-messages.md §4): the file is Ogg/Opus, mono, written here from
//! the packets of the recording and read back here. The drawn outline travels inside the file (a tag),
//! so a forwarded voice message keeps it.
//!
//! The receiver trusts none of it. The length is counted from the packets' own headers, never from what
//! the container or the offer claims: a file whose count disagrees with its offer, or that is not such
//! an Ogg at all, is a plain file and is never handed to a decoder.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use omni_types::{AttachmentPresentationKindV1, AttachmentPresentationV1, OmniError, OmniResult};

/// The longest recording.
pub const VOICE_MAX_MS: u32 = 15 * 60 * 1000;
/// How many bars the outline has.
pub const VOICE_WAVEFORM_BYTES: usize = 64;
/// What a receiver accepts as a voice message: 15 minutes well above the bitrate we record at.
pub const VOICE_RECEIVED_MAX_BYTES: i64 = 8 * 1024 * 1024;
/// A recording stops a little after the limit it was told; the offer's length may be this far off.
const VOICE_LENGTH_SLACK_MS: u32 = 5_000;
/// RFC 6716: the largest legal packet is 1275 bytes per frame; this is well over what one frame holds.
const VOICE_PACKET_MAX_BYTES: usize = 8 * 1024;

const OPUS_HEAD: &[u8; 8] = b"OpusHead";
const OPUS_TAGS: &[u8; 8] = b"OpusTags";
const WAVEFORM_TAG: &str = "OMNI_WAVEFORM=";
const OGG_CAPTURE: &[u8; 4] = b"OggS";
const OGG_STREAM_SERIAL: u32 = 0x4F4D_4E49;
/// The decoder's start-up samples at 48 kHz, as the encoder's look-ahead asks (RFC 7845 §4.2).
const OPUS_PRE_SKIP: u16 = 312;
const OGG_SEGMENTS_PER_PAGE: usize = 255;
const OGG_PACKETS_PER_PAGE: usize = 50;

/// A voice message as its file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceFile {
    pub packets: Vec<Vec<u8>>,
    pub duration_ms: u32,
    pub waveform: Vec<u8>,
}

impl VoiceFile {
    pub fn presentation(&self) -> AttachmentPresentationV1 {
        AttachmentPresentationV1 {
            kind: AttachmentPresentationKindV1::Voice,
            width: 0,
            height: 0,
            preview_jpeg_b64: None,
            duration_ms: Some(self.duration_ms),
            waveform_b64: (!self.waveform.is_empty()).then(|| B64.encode(&self.waveform)),
        }
    }
}

/// How many 48 kHz samples an Opus packet holds, from its own header (RFC 6716 §3.1). `None`: no valid
/// packet.
fn opus_packet_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = u32::from(toc >> 3);
    let frame = match config {
        0..=11 => [480, 960, 1920, 2880][(config % 4) as usize],
        12..=15 => [480, 960][(config % 2) as usize],
        _ => [120, 240, 480, 960][(config % 4) as usize],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3F),
    };
    let samples = frame * frames;
    // A packet is at most 120 ms.
    (frames > 0 && samples <= 5760).then_some(samples)
}

const fn ogg_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 0x8000_0000 != 0 { (value << 1) ^ 0x04C1_1DB7 } else { value << 1 };
            bit += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

static OGG_CRC_TABLE: [u32; 256] = ogg_crc_table();

fn ogg_crc(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |crc, byte| {
        (crc << 8) ^ OGG_CRC_TABLE[(((crc >> 24) as u8) ^ byte) as usize]
    })
}

fn push_ogg_page(out: &mut Vec<u8>, flags: u8, granule: u64, sequence: u32, packets: &[&[u8]]) {
    let start = out.len();
    out.extend_from_slice(OGG_CAPTURE);
    out.push(0);
    out.push(flags);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&OGG_STREAM_SERIAL.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    let crc_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let segments: usize = packets.iter().map(|packet| packet.len() / 255 + 1).sum();
    out.push(segments as u8);
    for packet in packets {
        out.extend(std::iter::repeat_n(255u8, packet.len() / 255));
        out.push((packet.len() % 255) as u8);
    }
    for packet in packets {
        out.extend_from_slice(packet);
    }
    let crc = ogg_crc(&out[start..]);
    out[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

/// The file of a recording: its Opus packets (mono) and the outline to draw.
pub fn write_voice_ogg(packets: &[Vec<u8>], waveform: &[u8]) -> OmniResult<Vec<u8>> {
    if packets.is_empty() {
        return Err(OmniError::InvalidInput("the recording is empty".to_string()));
    }
    if waveform.len() > VOICE_WAVEFORM_BYTES {
        return Err(OmniError::InvalidInput("the outline is too long".to_string()));
    }
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(OPUS_HEAD);
    head.push(1); // version
    head.push(1); // mono
    head.extend_from_slice(&OPUS_PRE_SKIP.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes()); // gain
    head.push(0); // channel mapping

    let vendor = b"Omni";
    let tag = format!("{WAVEFORM_TAG}{}", B64.encode(waveform));
    let mut tags = Vec::new();
    tags.extend_from_slice(OPUS_TAGS);
    tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&1u32.to_le_bytes());
    tags.extend_from_slice(&(tag.len() as u32).to_le_bytes());
    tags.extend_from_slice(tag.as_bytes());

    let mut out = Vec::new();
    push_ogg_page(&mut out, 0x02, 0, 0, &[&head]);
    push_ogg_page(&mut out, 0x00, 0, 1, &[&tags]);

    let mut sequence = 2u32;
    let mut granule = 0u64;
    let mut page: Vec<&[u8]> = Vec::new();
    let mut segments = 0usize;
    for (index, packet) in packets.iter().enumerate() {
        if packet.len() > VOICE_PACKET_MAX_BYTES {
            return Err(OmniError::InvalidInput("a recording's packet is too long".to_string()));
        }
        let samples = opus_packet_samples(packet)
            .ok_or_else(|| OmniError::InvalidInput("not an Opus packet".to_string()))?;
        let needed = packet.len() / 255 + 1;
        if !page.is_empty() && (segments + needed > OGG_SEGMENTS_PER_PAGE || page.len() >= OGG_PACKETS_PER_PAGE) {
            push_ogg_page(&mut out, 0x00, granule, sequence, &page);
            sequence += 1;
            page.clear();
            segments = 0;
        }
        page.push(packet);
        segments += needed;
        granule += u64::from(samples);
        if index + 1 == packets.len() {
            push_ogg_page(&mut out, 0x04, granule, sequence, &page);
        }
    }
    Ok(out)
}

fn not_voice(why: &str) -> OmniError {
    OmniError::InvalidInput(format!("not a voice message: {why}"))
}

/// Every packet of an Ogg stream, each page's checksum verified.
fn read_ogg_packets(bytes: &[u8]) -> OmniResult<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    let mut current = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let header = bytes.get(at..at + 27).ok_or_else(|| not_voice("a cut page"))?;
        if &header[..4] != OGG_CAPTURE || header[4] != 0 {
            return Err(not_voice("no Ogg page"));
        }
        let segment_count = usize::from(header[26]);
        let table = bytes
            .get(at + 27..at + 27 + segment_count)
            .ok_or_else(|| not_voice("a cut page"))?;
        let body_len: usize = table.iter().map(|value| usize::from(*value)).sum();
        let body_at = at + 27 + segment_count;
        let body = bytes.get(body_at..body_at + body_len).ok_or_else(|| not_voice("a cut page"))?;

        let mut page = bytes[at..body_at + body_len].to_vec();
        page[22..26].fill(0);
        if ogg_crc(&page).to_le_bytes() != header[22..26] {
            return Err(not_voice("a page's checksum is wrong"));
        }

        let mut from = 0usize;
        for lacing in table {
            let lacing = usize::from(*lacing);
            current.extend_from_slice(&body[from..from + lacing]);
            from += lacing;
            if current.len() > VOICE_PACKET_MAX_BYTES.max(OGG_SEGMENTS_PER_PAGE * 255) {
                return Err(not_voice("a packet is too long"));
            }
            if lacing < 255 {
                packets.push(std::mem::take(&mut current));
            }
        }
        at = body_at + body_len;
    }
    Ok(packets)
}

fn read_opus_head(head: &[u8]) -> OmniResult<u16> {
    if head.len() < 19 || &head[..8] != OPUS_HEAD || head[8] != 1 {
        return Err(not_voice("no Opus header"));
    }
    if head[9] != 1 || head[18] != 0 {
        return Err(not_voice("not mono"));
    }
    Ok(u16::from_le_bytes([head[10], head[11]]))
}

/// The outline from the tags. A broken or absent one is no reason to refuse the sound.
fn read_waveform_tag(tags: &[u8]) -> Vec<u8> {
    fn parse(tags: &[u8]) -> Option<Vec<u8>> {
        let read_u32 = |at: usize| -> Option<usize> {
            let raw: [u8; 4] = tags.get(at..at + 4)?.try_into().ok()?;
            Some(u32::from_le_bytes(raw) as usize)
        };
        if tags.get(..8)? != OPUS_TAGS {
            return None;
        }
        let mut at = 8 + 4 + read_u32(8)?;
        let count = read_u32(at)?;
        at += 4;
        for _ in 0..count {
            let len = read_u32(at)?;
            at += 4;
            let comment = tags.get(at..at.checked_add(len)?)?;
            at += len;
            if let Some(value) = comment.strip_prefix(WAVEFORM_TAG.as_bytes()) {
                let bytes = B64.decode(value).ok()?;
                return (bytes.len() <= VOICE_WAVEFORM_BYTES).then_some(bytes);
            }
        }
        None
    }
    parse(tags).unwrap_or_default()
}

/// A voice message's file, checked: a mono Ogg/Opus within the limits. The length is counted from the
/// packets themselves.
pub fn read_voice_ogg(bytes: &[u8]) -> OmniResult<VoiceFile> {
    if bytes.len() as i64 > VOICE_RECEIVED_MAX_BYTES {
        return Err(not_voice("too big"));
    }
    let mut all = read_ogg_packets(bytes)?.into_iter();
    let head = all.next().ok_or_else(|| not_voice("empty"))?;
    let pre_skip = read_opus_head(&head)?;
    let tags = all.next().ok_or_else(|| not_voice("no tags"))?;
    let waveform = read_waveform_tag(&tags);

    let max_samples = u64::from(VOICE_MAX_MS + VOICE_LENGTH_SLACK_MS) * 48;
    let mut samples = 0u64;
    let packets: Vec<Vec<u8>> = all.collect();
    for packet in &packets {
        samples += u64::from(opus_packet_samples(packet).ok_or_else(|| not_voice("a bad packet"))?);
        if samples > max_samples {
            return Err(not_voice("too long"));
        }
    }
    if packets.is_empty() {
        return Err(not_voice("no sound"));
    }
    let duration_ms = (samples.saturating_sub(u64::from(pre_skip)) / 48) as u32;
    Ok(VoiceFile { packets, duration_ms, waveform })
}

/// Packets between the shell and the core: each with its length ahead of it (2 bytes, little-endian).
pub fn join_voice_packets(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packets.iter().map(|packet| packet.len() + 2).sum());
    for packet in packets {
        out.extend_from_slice(&(packet.len() as u16).to_le_bytes());
        out.extend_from_slice(packet);
    }
    out
}

pub fn split_voice_packets(blob: &[u8]) -> OmniResult<Vec<Vec<u8>>> {
    let cut = || OmniError::InvalidInput("the recording's packets are cut".to_string());
    let mut packets = Vec::new();
    let mut at = 0usize;
    while at < blob.len() {
        let raw: [u8; 2] = blob.get(at..at + 2).ok_or_else(cut)?.try_into().map_err(|_| cut())?;
        let len = usize::from(u16::from_le_bytes(raw));
        at += 2;
        packets.push(blob.get(at..at + len).ok_or_else(cut)?.to_vec());
        at += len;
    }
    Ok(packets)
}

/// What a received offer may be shown as. `None` = a plain file (the offer itself still stands).
pub fn accept_received_voice_presentation(
    presentation: &AttachmentPresentationV1,
    file_size_bytes: i64,
) -> Option<AttachmentPresentationV1> {
    let duration_ms = presentation.duration_ms?;
    if duration_ms == 0
        || duration_ms > VOICE_MAX_MS + VOICE_LENGTH_SLACK_MS
        || file_size_bytes <= 0
        || file_size_bytes > VOICE_RECEIVED_MAX_BYTES
    {
        return None;
    }
    // A bad outline is dropped, the voice message stays: it draws evenly.
    let waveform_b64 = presentation.waveform_b64.as_deref().filter(|b64| {
        b64.len() <= VOICE_WAVEFORM_BYTES * 4 / 3 + 4 && B64.decode(b64).is_ok_and(|bytes| bytes.len() <= VOICE_WAVEFORM_BYTES)
    });
    Some(AttachmentPresentationV1 {
        kind: AttachmentPresentationKindV1::Voice,
        width: 0,
        height: 0,
        preview_jpeg_b64: None,
        duration_ms: Some(duration_ms),
        waveform_b64: waveform_b64.map(str::to_string),
    })
}

/// Whether a downloaded voice message (its plaintext bytes) is what its offer said.
pub fn downloaded_voice_matches(bytes: &[u8], presentation: &AttachmentPresentationV1) -> bool {
    let Some(claimed) = presentation.duration_ms else {
        return false;
    };
    read_voice_ogg(bytes).is_ok_and(|file| file.duration_ms.abs_diff(claimed) <= VOICE_LENGTH_SLACK_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `count` packets of 20 ms each; the code byte 72 is a SILK wideband 20 ms frame.
    fn packets(count: usize, len: usize) -> Vec<Vec<u8>> {
        (0..count).map(|i| { let mut p = vec![(i % 251) as u8; len]; p[0] = 72; p }).collect()
    }

    #[test]
    fn a_recording_reads_back_as_it_was_written() {
        let written = packets(260, 60);
        let waveform: Vec<u8> = (0..64).collect();
        let file = read_voice_ogg(&write_voice_ogg(&written, &waveform).unwrap()).unwrap();
        assert_eq!(file.packets, written);
        assert_eq!(file.waveform, waveform);
        assert_eq!(file.duration_ms, (260 * 960 - u32::from(OPUS_PRE_SKIP)) / 48);
    }

    #[test]
    fn a_packet_longer_than_a_lacing_value_survives() {
        let written = packets(3, 700);
        assert_eq!(read_voice_ogg(&write_voice_ogg(&written, &[]).unwrap()).unwrap().packets, written);
    }

    #[test]
    fn a_changed_byte_or_a_cut_file_is_no_voice_message() {
        let bytes = write_voice_ogg(&packets(100, 40), &[1, 2, 3]).unwrap();
        let mut changed = bytes.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(read_voice_ogg(&changed).is_err());
        assert!(read_voice_ogg(&bytes[..bytes.len() - 5]).is_err());
        assert!(read_voice_ogg(b"RIFF....WAVE").is_err());
    }

    #[test]
    fn the_length_is_counted_from_the_packets_not_the_offer() {
        let bytes = write_voice_ogg(&packets(500, 30), &[]).unwrap();
        let file = read_voice_ogg(&bytes).unwrap();
        let honest = file.presentation();
        assert!(downloaded_voice_matches(&bytes, &honest));
        let lying = AttachmentPresentationV1 { duration_ms: Some(600_000), ..honest };
        assert!(!downloaded_voice_matches(&bytes, &lying));
    }

    #[test]
    fn a_file_over_the_time_limit_is_refused() {
        // 120 ms frames, cheap to write: far over the limit in a tiny file.
        let long: Vec<Vec<u8>> = vec![vec![(11u8 << 3) | 3, 2, 0]; 10_000];
        let bytes = write_voice_ogg(&long, &[]).unwrap();
        assert!(read_voice_ogg(&bytes).is_err());
    }

    #[test]
    fn a_claim_over_the_limits_is_a_plain_file() {
        let good = write_voice_ogg(&packets(100, 30), &[9; 64]).map(|bytes| read_voice_ogg(&bytes).unwrap().presentation()).unwrap();
        assert!(accept_received_voice_presentation(&good, 3_000).is_some());
        assert!(accept_received_voice_presentation(&good, VOICE_RECEIVED_MAX_BYTES + 1).is_none());
        let endless = AttachmentPresentationV1 { duration_ms: Some(VOICE_MAX_MS * 2), ..good.clone() };
        assert!(accept_received_voice_presentation(&endless, 3_000).is_none());
        let none = AttachmentPresentationV1 { duration_ms: None, ..good.clone() };
        assert!(accept_received_voice_presentation(&none, 3_000).is_none());
        let bad_outline = AttachmentPresentationV1 { waveform_b64: Some(B64.encode([1u8; 65])), ..good };
        let kept = accept_received_voice_presentation(&bad_outline, 3_000).expect("the sound stays");
        assert!(kept.waveform_b64.is_none());
    }

    #[test]
    fn the_shells_packets_split_as_they_were_joined() {
        let written = packets(5, 20);
        assert_eq!(split_voice_packets(&join_voice_packets(&written)).unwrap(), written);
        assert!(split_voice_packets(&[200, 0, 1]).is_err());
    }
}
