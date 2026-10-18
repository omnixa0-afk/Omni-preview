//! Which video codec goes from one participant to another — per pair, per direction, and the
//! sender's stream plan in a group. Pure functions over the capability passports; the orchestration
//! (who announces what, when a plan is recomputed) lives in `omni-app`.
//!
//! The design and its reasons: `docs/decisions/per-pair-video-codec-selection.md`. In short — the
//! SENDER decides (encoding is the expensive part), the score is "compression gain minus the encode
//! cost at the sender minus the decode cost at the receiver", among codecs the sender can encode and
//! the receiver can decode under both users' ceilings. A software encoder is used only when the
//! sender has no hardware encoder the receiver can take. H.264 is the floor almost everybody decodes:
//! it is hardware-only (no software H.264 in the build), so a machine without a Media Foundation
//! H.264 decoder does not have it — AV1 in software is its way in.
//!
//! The numbers below are points on one scale (bigger = better). They are a starting table to be
//! tuned by our own measurements, not truths: the ADR says so, and the tests pin only the orderings
//! that follow from the ADR, not the exact values.

use serde::{Deserialize, Serialize};

use crate::{RealtimeMediaCapabilitiesV1, RealtimeVideoCodec, RealtimeVideoCodecCapabilityV1};

/// What the picture is. Screen content has text and flat areas where AV1's screen tools win big
/// and hardware HEVC barely helps (ADR §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoContent {
    #[default]
    Camera,
    Screen,
}

/// The shape of the stream being planned: what the codec has to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoStreamShape {
    pub content: VideoContent,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The bitrate the stream would get in H.264. Gains shrink when bits are plentiful.
    pub h264_bitrate_bps: u32,
}

impl VideoStreamShape {
    fn pixels_per_second(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * u64::from(self.fps.max(1))
    }

    /// Bits per pixel in thousandths, at the H.264 bitrate.
    fn milli_bits_per_pixel(&self) -> u64 {
        let pps = self.pixels_per_second().max(1);
        u64::from(self.h264_bitrate_bps) * 1000 / pps
    }
}

/// Default RTP payload types per codec (dynamic range). Both sides use the same table, so a
/// passport only needs to say "I can".
pub fn default_video_payload_type(codec: RealtimeVideoCodec) -> u8 {
    match codec {
        RealtimeVideoCodec::H264 => 102,
        RealtimeVideoCodec::H265 => 49,
        RealtimeVideoCodec::Av1 => 45,
    }
}

/// Order when everything else is equal (ADR §3): AV1 > HEVC > H.264. Also the scale a user's
/// ceiling is measured on.
pub fn video_codec_rank(codec: RealtimeVideoCodec) -> u8 {
    match codec {
        RealtimeVideoCodec::H264 => 1,
        RealtimeVideoCodec::H265 => 2,
        RealtimeVideoCodec::Av1 => 3,
    }
}

/// How many bits a codec needs relative to H.264 for the same picture, in percent. Used to give
/// each encoding its own target and to count the uplink budget.
pub fn video_codec_bitrate_percent(codec: RealtimeVideoCodec) -> u32 {
    match codec {
        RealtimeVideoCodec::H264 => 100,
        RealtimeVideoCodec::H265 => 70,
        RealtimeVideoCodec::Av1 => 60,
    }
}

pub const ALL_VIDEO_CODECS: [RealtimeVideoCodec; 3] = [
    RealtimeVideoCodec::Av1,
    RealtimeVideoCodec::H265,
    RealtimeVideoCodec::H264,
];

/// The codec a sender will use towards one receiver, and why — the "why" is shown in
/// Diagnostics → Media (ADR §8) and written to the trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoCodecChoice {
    pub codec: RealtimeVideoCodec,
    pub score: i32,
    pub reason: String,
}

fn codec_entry(
    passport: &RealtimeMediaCapabilitiesV1,
    codec: RealtimeVideoCodec,
) -> Option<&RealtimeVideoCodecCapabilityV1> {
    passport.codecs.iter().find(|entry| entry.codec == codec)
}

fn above_ceiling(passport: &RealtimeMediaCapabilitiesV1, codec: RealtimeVideoCodec) -> bool {
    passport
        .codec_ceiling
        .is_some_and(|ceiling| video_codec_rank(codec) > video_codec_rank(ceiling))
}

fn fits(side: &crate::RealtimeVideoCodecSideV1, shape: &VideoStreamShape) -> bool {
    (side.max_width == 0 || shape.width <= side.max_width)
        && (side.max_height == 0 || shape.height <= side.max_height)
        && (side.max_fps == 0 || shape.fps <= side.max_fps)
}

/// Compression gain over H.264 for this picture, before the encoder vendor's correction.
fn compression_gain(codec: RealtimeVideoCodec, shape: &VideoStreamShape) -> i32 {
    let base = match (codec, shape.content) {
        (RealtimeVideoCodec::H264, _) => 0,
        (RealtimeVideoCodec::H265, VideoContent::Camera) => 30,
        (RealtimeVideoCodec::H265, VideoContent::Screen) => 10,
        (RealtimeVideoCodec::Av1, VideoContent::Camera) => 40,
        (RealtimeVideoCodec::Av1, VideoContent::Screen) => 60,
    };
    // Plenty of bits (≥ 0.12 bit/pixel at H.264) — every codec looks good, the gain halves.
    if shape.milli_bits_per_pixel() >= 120 {
        base / 2
    } else {
        base
    }
}

/// Hardware encoders of one codec differ (NVENC AV1 is good, early AMD AV1 is not, ADR §3).
/// `implementation` is the passport's free-form name of the encoder ("nvidia", "amd", "intel",
/// "libaom", "mediafoundation/…").
fn encoder_quality_adjust(codec: RealtimeVideoCodec, implementation: Option<&str>) -> i32 {
    let name = implementation.unwrap_or_default().to_ascii_lowercase();
    match codec {
        RealtimeVideoCodec::Av1 if name.contains("amd") => -20,
        RealtimeVideoCodec::H265 if name.contains("amd") => -5,
        _ => 0,
    }
}

/// Cost of encoding at the sender; `None` = must not encode this way at all.
fn encode_cost(
    sender: &RealtimeMediaCapabilitiesV1,
    codec: RealtimeVideoCodec,
    hardware: bool,
    shape: &VideoStreamShape,
) -> Option<i32> {
    let battery = sender.power == crate::RealtimePowerSource::Battery;
    if hardware {
        return Some(if battery { 8 } else { 5 });
    }
    // Software encoding: never H.264 (docs/decisions/h264-runtime-proof.md — software H.264 must
    // fail visibly, not run silently), never under high load, and only small pictures.
    if codec == RealtimeVideoCodec::H264 {
        return None;
    }
    let pps = shape.pixels_per_second();
    let small = pps <= 1280 * 720 * 30;
    let tiny = pps <= 640 * 360 * 30;
    let base = match sender.cpu_load {
        crate::RealtimeCpuLoad::Low if small => 25,
        crate::RealtimeCpuLoad::Medium if tiny => 40,
        _ => return None,
    };
    Some(if battery { base + 20 } else { base })
}

/// Cost of decoding at the receiver.
fn decode_cost(
    receiver: &RealtimeMediaCapabilitiesV1,
    codec: RealtimeVideoCodec,
    hardware: bool,
    shape: &VideoStreamShape,
) -> i32 {
    if hardware {
        return 2;
    }
    let base = match codec {
        RealtimeVideoCodec::H264 => 5,
        RealtimeVideoCodec::Av1 => 10, // dav1d
        RealtimeVideoCodec::H265 => 15,
    };
    let big = shape.pixels_per_second() > 1280 * 720 * 30;
    let base = if big { base * 2 } else { base };
    let base = match receiver.cpu_load {
        crate::RealtimeCpuLoad::High => base * 2,
        _ => base,
    };
    if receiver.power == crate::RealtimePowerSource::Battery {
        // Battery makes software decoding the expensive kind (ADR §3): a receiver on battery takes
        // hardware HEVC over software AV1.
        base * 3
    } else {
        base
    }
}

/// Whether the sender can reach this receiver with SOME hardware encoder: a codec it encodes in
/// hardware and the receiver decodes, within both ceilings and the picture's size.
fn has_hardware_path(
    sender: &RealtimeMediaCapabilitiesV1,
    receiver: &RealtimeMediaCapabilitiesV1,
    shape: &VideoStreamShape,
) -> bool {
    ALL_VIDEO_CODECS.iter().any(|codec| {
        !above_ceiling(sender, *codec)
            && !above_ceiling(receiver, *codec)
            && codec_entry(sender, *codec)
                .and_then(|entry| entry.encode.as_ref())
                .is_some_and(|side| side.hardware && fits(side, shape))
            && codec_entry(receiver, *codec)
                .and_then(|entry| entry.decode.as_ref())
                .is_some_and(|side| fits(side, shape))
    })
}

/// Score of one codec for the pair, or `None` when this pair cannot use it.
pub fn score_video_codec(
    sender: &RealtimeMediaCapabilitiesV1,
    receiver: &RealtimeMediaCapabilitiesV1,
    codec: RealtimeVideoCodec,
    shape: &VideoStreamShape,
) -> Option<i32> {
    if above_ceiling(sender, codec) || above_ceiling(receiver, codec) {
        return None;
    }
    let send = codec_entry(sender, codec)?;
    let encode = send.encode.as_ref().filter(|side| fits(side, shape))?;
    // Software encoding only when there is no hardware way to this receiver at all: an "idle"
    // processor says nothing about keeping up with a real-time encode. Live 2026-10-18: a Ryzen 5
    // 2600 with hardware HEVC/H.264 took software AV1 for a screen and fell to ~3 fps
    // (docs/bugs/software-av1-screen-starves-on-mid-cpu.md).
    if !encode.hardware && has_hardware_path(sender, receiver, shape) {
        return None;
    }
    let decode = codec_entry(receiver, codec)?
        .decode
        .as_ref()
        .filter(|side| fits(side, shape))?;
    let encode_cost = encode_cost(sender, codec, encode.hardware, shape)?;
    Some(
        compression_gain(codec, shape)
            + encoder_quality_adjust(codec, send.implementation.as_deref())
            - encode_cost
            - decode_cost(receiver, codec, decode.hardware, shape),
    )
}

/// Every codec this pair can use, best first (ties: AV1 > HEVC > H.264).
pub fn rank_video_codecs(
    sender: &RealtimeMediaCapabilitiesV1,
    receiver: &RealtimeMediaCapabilitiesV1,
    shape: &VideoStreamShape,
) -> Vec<(RealtimeVideoCodec, i32)> {
    let mut ranked: Vec<(RealtimeVideoCodec, i32)> = ALL_VIDEO_CODECS
        .iter()
        .filter_map(|codec| {
            score_video_codec(sender, receiver, *codec, shape).map(|score| (*codec, score))
        })
        .collect();
    ranked.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| video_codec_rank(*right).cmp(&video_codec_rank(*left)))
    });
    ranked
}

fn codec_label(codec: RealtimeVideoCodec) -> &'static str {
    match codec {
        RealtimeVideoCodec::H264 => "H.264",
        RealtimeVideoCodec::H265 => "HEVC",
        RealtimeVideoCodec::Av1 => "AV1",
    }
}

/// Why the best codec (AV1) was not chosen, in words for Diagnostics → Media.
fn why_not_better(
    sender: &RealtimeMediaCapabilitiesV1,
    receiver: &RealtimeMediaCapabilitiesV1,
    chosen: RealtimeVideoCodec,
    shape: &VideoStreamShape,
) -> String {
    for better in ALL_VIDEO_CODECS
        .iter()
        .copied()
        .filter(|codec| video_codec_rank(*codec) > video_codec_rank(chosen))
    {
        let label = codec_label(better);
        if above_ceiling(sender, better) || above_ceiling(receiver, better) {
            return format!("{label} выше потолка в настройках");
        }
        let encodes = codec_entry(sender, better).and_then(|entry| entry.encode.as_ref());
        let Some(encode) = encodes else {
            return format!("отправитель не кодирует {label}");
        };
        let decodes = codec_entry(receiver, better).and_then(|entry| entry.decode.as_ref());
        if decodes.is_none() {
            return format!("получатель не декодирует {label}");
        }
        if !encode.hardware && has_hardware_path(sender, receiver, shape) {
            return format!("{label} только программно, а у отправителя есть аппаратный кодер");
        }
        if !encode.hardware && encode_cost(sender, better, false, shape).is_none() {
            return format!("{label} только программно, а процессор занят или картинка велика");
        }
        if score_video_codec(sender, receiver, better, shape).is_some() {
            return format!("{label} дороже для этой пары");
        }
    }
    "лучший из возможных".to_string()
}

/// The codec for one direction S → R (1:1, or one pair of a group before the budget).
pub fn choose_video_codec(
    sender: &RealtimeMediaCapabilitiesV1,
    receiver: &RealtimeMediaCapabilitiesV1,
    shape: &VideoStreamShape,
) -> Option<VideoCodecChoice> {
    let (codec, score) = rank_video_codecs(sender, receiver, shape).into_iter().next()?;
    let reason = if codec == RealtimeVideoCodec::Av1 {
        "лучший из возможных".to_string()
    } else {
        why_not_better(sender, receiver, codec, shape)
    };
    Some(VideoCodecChoice {
        codec,
        score,
        reason,
    })
}

/// Switching costs a key frame; a pair changes codec at most once per 10 s, and only for a real
/// gain (ADR §6).
pub const VIDEO_CODEC_SWITCH_MIN_INTERVAL_MS: i64 = 10_000;
pub const VIDEO_CODEC_SWITCH_MIN_GAIN: i32 = 10;

pub fn should_switch_video_codec(
    current_score: Option<i32>,
    candidate_score: i32,
    ms_since_last_switch: i64,
) -> bool {
    match current_score {
        // The current codec became impossible (failed encoder, a receiver that cannot decode it,
        // a lowered ceiling): switch now, whatever the clock says.
        None => true,
        Some(current) => {
            ms_since_last_switch >= VIDEO_CODEC_SWITCH_MIN_INTERVAL_MS
                && candidate_score - current >= VIDEO_CODEC_SWITCH_MIN_GAIN
        }
    }
}

/// What a sender may spend on encodings at once (ADR §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoEncodeBudget {
    /// Hardware encoder sessions left (consumer NVIDIA: about 8 for everything, screen share too).
    pub hardware_sessions: u32,
    /// Software encodings the processor can carry now (0 under high load).
    pub software_encodings: u32,
    pub uplink_bps: u64,
}

/// One encoding of a sender's plan: one codec, the receivers that get it, its own target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedVideoEncoding {
    pub codec: RealtimeVideoCodec,
    pub hardware: bool,
    pub target_bitrate_bps: u32,
    pub receivers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoStreamPlan {
    pub encodings: Vec<PlannedVideoEncoding>,
    /// Watchers this sender cannot serve with any codec at all: H.264 is the usual floor, but it is
    /// hardware-only on both sides, so a sender or a watcher without it has no floor to share.
    pub unserved: Vec<String>,
}

impl VideoStreamPlan {
    pub fn codec_for(&self, receiver: &str) -> Option<RealtimeVideoCodec> {
        self.encodings
            .iter()
            .find(|encoding| encoding.receivers.iter().any(|id| id == receiver))
            .map(|encoding| encoding.codec)
    }
}

fn encodes_in_hardware(sender: &RealtimeMediaCapabilitiesV1, codec: RealtimeVideoCodec) -> bool {
    codec_entry(sender, codec)
        .and_then(|entry| entry.encode.as_ref())
        .is_some_and(|side| side.hardware)
}

fn plan_cost(
    sender: &RealtimeMediaCapabilitiesV1,
    codecs: &[RealtimeVideoCodec],
    shape: &VideoStreamShape,
) -> (u32, u32, u64) {
    let mut hardware = 0;
    let mut software = 0;
    let mut uplink = 0u64;
    for codec in codecs {
        if encodes_in_hardware(sender, *codec) {
            hardware += 1;
        } else {
            software += 1;
        }
        uplink += u64::from(encoding_bitrate(*codec, shape));
    }
    (hardware, software, uplink)
}

fn encoding_bitrate(codec: RealtimeVideoCodec, shape: &VideoStreamShape) -> u32 {
    shape.h264_bitrate_bps / 100 * video_codec_bitrate_percent(codec)
}

/// A sender's stream plan for a group call (ADR §4): the best codec per watcher, at most one
/// encoding per codec, then — while the plan does not fit the budget — drop the encoding whose
/// loss is smallest and move its watchers to the best codec still in the plan.
///
/// `watchers` are the receivers that actually look at this sender now (active-speaker gating, C1):
/// a sender nobody watches gets an empty plan and encodes nothing (ADR §4.5).
pub fn plan_video_streams(
    sender: &RealtimeMediaCapabilitiesV1,
    watchers: &[(String, &RealtimeMediaCapabilitiesV1)],
    shape: &VideoStreamShape,
    budget: VideoEncodeBudget,
) -> VideoStreamPlan {
    let ranked: Vec<(&String, Vec<(RealtimeVideoCodec, i32)>)> = watchers
        .iter()
        .map(|(id, receiver)| (id, rank_video_codecs(sender, receiver, shape)))
        .collect();

    let mut in_plan: Vec<RealtimeVideoCodec> = Vec::new();
    for (_, options) in &ranked {
        if let Some((codec, _)) = options.first() {
            if !in_plan.contains(codec) {
                in_plan.push(*codec);
            }
        }
    }

    // Score of a watcher given the codecs still in the plan (its best one among them).
    let best_in = |options: &[(RealtimeVideoCodec, i32)], codecs: &[RealtimeVideoCodec]| {
        options
            .iter()
            .find(|(codec, _)| codecs.contains(codec))
            .map(|(_, score)| *score)
    };
    // A plan's total; a watcher left without any codec weighs like a lost picture.
    const LOST: i32 = -1000;
    let total = |codecs: &[RealtimeVideoCodec]| -> i32 {
        ranked
            .iter()
            .map(|(_, options)| best_in(options, codecs).unwrap_or(LOST))
            .sum()
    };

    // Dropped codecs never come back, so the loop ends: at most one round per codec.
    let mut dropped: Vec<RealtimeVideoCodec> = Vec::new();
    loop {
        let (hardware, software, uplink) = plan_cost(sender, &in_plan, shape);
        let fits = hardware <= budget.hardware_sessions
            && software <= budget.software_encodings
            && uplink <= budget.uplink_bps;
        if fits || in_plan.len() <= 1 {
            break;
        }
        // Drop the encoding whose removal costs the least; ties drop the worse codec first.
        let victim = in_plan
            .iter()
            .copied()
            .max_by_key(|candidate| {
                let rest: Vec<RealtimeVideoCodec> =
                    in_plan.iter().copied().filter(|c| c != candidate).collect();
                (total(&rest), std::cmp::Reverse(video_codec_rank(*candidate)))
            })
            .expect("plan has more than one encoding");
        in_plan.retain(|codec| *codec != victim);
        dropped.push(victim);
        // A watcher whose remaining options are all gone takes its next best codec, even one
        // outside the plan — H.264 at the end of everyone's list keeps the floor.
        for (_, options) in &ranked {
            if best_in(options, &in_plan).is_none() {
                if let Some((codec, _)) = options
                    .iter()
                    .find(|(codec, _)| !dropped.contains(codec) && !in_plan.contains(codec))
                {
                    in_plan.push(*codec);
                }
            }
        }
    }

    let mut plan = VideoStreamPlan::default();
    for (id, options) in &ranked {
        match options.iter().find(|(codec, _)| in_plan.contains(codec)) {
            Some((codec, _)) => {
                match plan.encodings.iter_mut().find(|e| e.codec == *codec) {
                    Some(encoding) => encoding.receivers.push((*id).clone()),
                    None => plan.encodings.push(PlannedVideoEncoding {
                        codec: *codec,
                        hardware: encodes_in_hardware(sender, *codec),
                        target_bitrate_bps: encoding_bitrate(*codec, shape),
                        receivers: vec![(*id).clone()],
                    }),
                }
            }
            None => plan.unserved.push((*id).clone()),
        }
    }
    plan.encodings
        .sort_by_key(|encoding| std::cmp::Reverse(video_codec_rank(encoding.codec)));
    plan
}

/// The ONE codec a group sender encodes for everybody watching (ADR §9, phase 4a: one encoding per
/// sender until the stream label rides the group envelope). The plan of §4 with room for a single
/// encoding — a hardware one if the sender has it, else one software — which is also the ADR's own
/// limit case: "одна кодировка, которую примут все". H.264 when nobody watches yet, when a watcher's
/// passport is unknown (the caller says so by passing none), or when no single codec serves all.
pub fn choose_group_video_codec(
    sender: &RealtimeMediaCapabilitiesV1,
    watchers: &[(String, &RealtimeMediaCapabilitiesV1)],
    shape: &VideoStreamShape,
) -> RealtimeVideoCodec {
    if watchers.is_empty() {
        return RealtimeVideoCodec::H264;
    }
    for budget in [
        VideoEncodeBudget { hardware_sessions: 1, software_encodings: 0, uplink_bps: u64::MAX },
        VideoEncodeBudget { hardware_sessions: 0, software_encodings: 1, uplink_bps: u64::MAX },
    ] {
        let plan = plan_video_streams(sender, watchers, shape, budget);
        if let [only] = plan.encodings.as_slice() {
            if plan.unserved.is_empty() && only.receivers.len() == watchers.len() {
                return only.codec;
            }
        }
    }
    RealtimeVideoCodec::H264
}

#[cfg(test)]
mod tests;
