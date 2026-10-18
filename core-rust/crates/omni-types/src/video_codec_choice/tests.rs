use super::*;
use crate::{RealtimeCpuLoad, RealtimePowerSource, RealtimeVideoCodecSideV1};

const HW: Option<bool> = Some(true);
const SW: Option<bool> = Some(false);
const NO: Option<bool> = None;

fn side(hardware: Option<bool>) -> Option<RealtimeVideoCodecSideV1> {
    hardware.map(|hardware| RealtimeVideoCodecSideV1 {
        hardware,
        max_width: 0,
        max_height: 0,
        max_fps: 0,
    })
}

/// A passport: per codec (encode, decode), in the order AV1, HEVC, H.264.
fn passport(id: &str, av1: (Option<bool>, Option<bool>), hevc: (Option<bool>, Option<bool>), h264: (Option<bool>, Option<bool>)) -> RealtimeMediaCapabilitiesV1 {
    let mut codecs = Vec::new();
    for (codec, (encode, decode)) in [
        (RealtimeVideoCodec::Av1, av1),
        (RealtimeVideoCodec::H265, hevc),
        (RealtimeVideoCodec::H264, h264),
    ] {
        if encode.is_some() || decode.is_some() {
            codecs.push(crate::RealtimeVideoCodecCapabilityV1 {
                codec,
                payload_type: default_video_payload_type(codec),
                encode: side(encode),
                decode: side(decode),
                implementation: Some("nvidia".to_string()),
            });
        }
    }
    RealtimeMediaCapabilitiesV1 {
        dialog_key: "d".to_string(),
        capabilities_id: id.to_string(),
        codecs,
        power: RealtimePowerSource::Mains,
        cpu_load: RealtimeCpuLoad::Medium,
        codec_ceiling: None,
        bitrate_level: Default::default(),
        content: VideoContent::Camera,
        video: None,
        max_datagram_bytes: 1200,
        supports_rtcp_feedback: true,
        sent_at_unix_ms: 0,
    }
}

/// The stand's laptop: RTX 4060 — NVENC H.264/HEVC/AV1, decodes all three in hardware.
fn laptop() -> RealtimeMediaCapabilitiesV1 {
    passport("laptop", (HW, HW), (HW, HW), (HW, HW))
}

/// The stand's PC: RTX 3060 Ti — NVENC H.264/HEVC; AV1 decodes in hardware but encodes only in
/// software (libaom in the build) — what its passport really says (`sw=av1`).
fn pc() -> RealtimeMediaCapabilitiesV1 {
    let mut pc = passport("pc", (SW, HW), (HW, HW), (HW, HW));
    pc.cpu_load = RealtimeCpuLoad::Low; // idle, as it reported on the live call
    pc
}

/// No video hardware at all, only Omni's software decoders (dav1d, H.264); no HEVC decoder.
fn bare() -> RealtimeMediaCapabilitiesV1 {
    passport("bare", (NO, SW), (NO, NO), (NO, SW))
}

fn camera_720p() -> VideoStreamShape {
    VideoStreamShape {
        content: VideoContent::Camera,
        width: 1280,
        height: 720,
        fps: 30,
        h264_bitrate_bps: 2_500_000,
    }
}

fn screen_1080p() -> VideoStreamShape {
    VideoStreamShape {
        content: VideoContent::Screen,
        width: 1920,
        height: 1080,
        fps: 15,
        h264_bitrate_bps: 2_500_000,
    }
}

fn chosen(sender: &RealtimeMediaCapabilitiesV1, receiver: &RealtimeMediaCapabilitiesV1, shape: VideoStreamShape) -> RealtimeVideoCodec {
    choose_video_codec(sender, receiver, &shape).expect("a codec").codec
}

#[test]
fn the_stand_runs_av1_from_the_laptop_and_hevc_from_the_pc() {
    assert_eq!(chosen(&laptop(), &pc(), camera_720p()), RealtimeVideoCodec::Av1);
    let back = choose_video_codec(&pc(), &laptop(), &camera_720p()).unwrap();
    assert_eq!(back.codec, RealtimeVideoCodec::H265);
    assert!(back.reason.contains("есть аппаратный кодер"), "{}", back.reason);
}

/// Live 2026-10-18: the PC (hardware HEVC/H.264, AV1 only in software, "idle") took software AV1
/// for a screen and fell to ~3 fps. A software encoder never wins over a hardware one the receiver
/// can take — not for a screen either, however idle the processor says it is.
#[test]
fn software_av1_never_wins_over_a_hardware_encoder_even_for_a_screen() {
    let small_screen = VideoStreamShape { width: 1280, height: 720, ..screen_1080p() };
    assert_eq!(chosen(&pc(), &laptop(), small_screen), RealtimeVideoCodec::H265);
    assert_eq!(chosen(&pc(), &laptop(), camera_720p()), RealtimeVideoCodec::H265);

    // No hardware encoder at all: software AV1 is the way, for a screen on an idle processor.
    let mut soft_only = passport("s", (SW, SW), (NO, NO), (NO, NO));
    soft_only.cpu_load = RealtimeCpuLoad::Low;
    assert_eq!(chosen(&soft_only, &laptop(), small_screen), RealtimeVideoCodec::Av1);

    // Hardware the receiver cannot take is no hardware path: HEVC-only hardware to a receiver
    // without an HEVC decoder leaves software AV1 as the only picture.
    let mut hevc_only = passport("s", (SW, SW), (HW, HW), (NO, NO));
    hevc_only.cpu_load = RealtimeCpuLoad::Low;
    assert_eq!(chosen(&hevc_only, &bare(), small_screen), RealtimeVideoCodec::Av1);
}

#[test]
fn everyone_decodes_av1_so_a_bare_receiver_still_gets_the_best_codec() {
    assert_eq!(chosen(&laptop(), &bare(), camera_720p()), RealtimeVideoCodec::Av1);
    // ...and from the PC (no AV1 encoder, the receiver has no HEVC decoder) the floor: H.264.
    let floor = choose_video_codec(&pc(), &bare(), &camera_720p()).unwrap();
    assert_eq!(floor.codec, RealtimeVideoCodec::H264);
    assert!(floor.reason.contains("AV1"), "{}", floor.reason);
}

#[test]
fn a_receiver_on_battery_takes_hardware_hevc_over_software_av1() {
    let mut receiver = passport("r", (NO, SW), (NO, HW), (NO, HW));
    assert_eq!(chosen(&laptop(), &receiver, camera_720p()), RealtimeVideoCodec::Av1);
    receiver.power = RealtimePowerSource::Battery;
    assert_eq!(chosen(&laptop(), &receiver, camera_720p()), RealtimeVideoCodec::H265);
}

#[test]
fn software_encoding_never_runs_under_high_load_or_for_h264() {
    let mut sender = passport("s", (SW, SW), (NO, NO), (NO, HW));
    sender.cpu_load = RealtimeCpuLoad::High;
    assert!(choose_video_codec(&sender, &laptop(), &camera_720p()).is_none());
    // Software H.264 is never a choice (h264-runtime-proof): without hardware H.264 and with a
    // busy processor there is simply no video from this sender.
    let sender = passport("s", (NO, SW), (NO, NO), (SW, SW));
    assert!(choose_video_codec(&sender, &laptop(), &camera_720p()).is_none());
}

#[test]
fn a_ceiling_on_either_side_crosses_out_the_codecs_above_it() {
    let mut receiver = laptop();
    receiver.codec_ceiling = Some(RealtimeVideoCodec::H264);
    let choice = choose_video_codec(&laptop(), &receiver, &camera_720p()).unwrap();
    assert_eq!(choice.codec, RealtimeVideoCodec::H264);
    assert!(choice.reason.contains("потолк"), "{}", choice.reason);
    let mut sender = laptop();
    sender.codec_ceiling = Some(RealtimeVideoCodec::H265);
    assert_eq!(chosen(&sender, &laptop(), camera_720p()), RealtimeVideoCodec::H265);
}

#[test]
fn limits_cross_out_a_codec_the_picture_does_not_fit() {
    let mut receiver = laptop();
    for entry in &mut receiver.codecs {
        if entry.codec == RealtimeVideoCodec::Av1 {
            entry.decode.as_mut().unwrap().max_width = 1280;
        }
    }
    assert_eq!(chosen(&laptop(), &receiver, camera_720p()), RealtimeVideoCodec::Av1);
    assert_eq!(chosen(&laptop(), &receiver, screen_1080p()), RealtimeVideoCodec::H265);
}

#[test]
fn a_weak_hardware_av1_encoder_loses_to_hevc_from_the_same_vendor() {
    let mut sender = laptop();
    for entry in &mut sender.codecs {
        entry.implementation = Some("AMD AMF".to_string());
    }
    assert_eq!(chosen(&sender, &laptop(), camera_720p()), RealtimeVideoCodec::H265);
}

#[test]
fn plenty_of_bits_shrinks_the_gain_but_not_the_order() {
    let rich = VideoStreamShape { h264_bitrate_bps: 20_000_000, ..camera_720p() };
    let ranked = rank_video_codecs(&laptop(), &laptop(), &rich);
    let lean = rank_video_codecs(&laptop(), &laptop(), &camera_720p());
    assert_eq!(ranked[0].0, RealtimeVideoCodec::Av1);
    assert!(ranked[0].1 < lean[0].1);
}

#[test]
fn ties_go_av1_then_hevc_then_h264() {
    // Same score for everything: no gain anywhere (plenty of bits halves 0 to 0 for H.264 only),
    // so compare equal scores directly.
    let mut ranked = [(RealtimeVideoCodec::H264, 5), (RealtimeVideoCodec::Av1, 5), (RealtimeVideoCodec::H265, 5)];
    ranked.sort_by(|(l, ls), (r, rs)| rs.cmp(ls).then_with(|| video_codec_rank(*r).cmp(&video_codec_rank(*l))));
    assert_eq!(ranked.iter().map(|(c, _)| *c).collect::<Vec<_>>(), ALL_VIDEO_CODECS.to_vec());
}

#[test]
fn a_pair_switches_at_most_every_ten_seconds_and_only_for_a_real_gain() {
    assert!(!should_switch_video_codec(Some(20), 25, 60_000)); // gain too small
    assert!(!should_switch_video_codec(Some(5), 40, 9_999)); // too soon
    assert!(should_switch_video_codec(Some(5), 40, 10_000));
    // The current codec broke: switch now.
    assert!(should_switch_video_codec(None, -5, 0));
}

fn group_watchers(passports: &[RealtimeMediaCapabilitiesV1]) -> Vec<(String, &RealtimeMediaCapabilitiesV1)> {
    passports.iter().map(|p| (p.capabilities_id.clone(), p)).collect()
}

const ROOMY: VideoEncodeBudget = VideoEncodeBudget {
    hardware_sessions: 8,
    software_encodings: 1,
    uplink_bps: 50_000_000,
};

#[test]
fn nobody_watching_means_nothing_is_encoded() {
    let plan = plan_video_streams(&laptop(), &[], &camera_720p(), ROOMY);
    assert!(plan.encodings.is_empty() && plan.unserved.is_empty());
}

/// A hundred people with mixed hardware (ADR §0): every watcher is served, there is at most one
/// encoding per codec, and with AV1 decodable by all the common case is ONE encoding.
#[test]
fn a_hundred_mixed_participants_share_at_most_one_encoding_per_codec() {
    let mut people = Vec::new();
    for index in 0..99 {
        let mut person = match index % 3 {
            0 => laptop(),
            1 => pc(),
            _ => bare(),
        };
        person.capabilities_id = format!("p{index}");
        people.push(person);
    }
    let plan = plan_video_streams(&laptop(), &group_watchers(&people), &camera_720p(), ROOMY);
    assert!(plan.unserved.is_empty());
    assert_eq!(plan.encodings.len(), 1);
    assert_eq!(plan.encodings[0].codec, RealtimeVideoCodec::Av1);
    assert_eq!(plan.encodings[0].receivers.len(), 99);

    // Some of them on battery with hardware HEVC: they get their own HEVC encoding.
    for person in people.iter_mut().step_by(5) {
        person.power = RealtimePowerSource::Battery;
        person.codecs.retain(|c| c.codec != RealtimeVideoCodec::Av1);
        person.codecs.push(crate::RealtimeVideoCodecCapabilityV1 {
            codec: RealtimeVideoCodec::Av1,
            payload_type: 45,
            encode: None,
            decode: side(SW),
            implementation: None,
        });
    }
    let plan = plan_video_streams(&laptop(), &group_watchers(&people), &camera_720p(), ROOMY);
    let codecs: Vec<_> = plan.encodings.iter().map(|e| e.codec).collect();
    assert!(codecs.contains(&RealtimeVideoCodec::Av1) && codecs.contains(&RealtimeVideoCodec::H265));
    let mut seen = std::collections::HashSet::new();
    assert!(codecs.iter().all(|c| seen.insert(*c)), "one encoding per codec: {codecs:?}");
    let served: usize = plan.encodings.iter().map(|e| e.receivers.len()).sum();
    assert_eq!(served, 99);
}

#[test]
fn over_budget_the_cheapest_encoding_goes_and_its_watchers_move() {
    // From the PC: HEVC to those who decode it, H.264 to the bare ones.
    let mut people = vec![laptop(), pc(), bare()];
    for (index, person) in people.iter_mut().enumerate() {
        person.capabilities_id = format!("p{index}");
    }
    let watchers = group_watchers(&people);
    let plan = plan_video_streams(&pc(), &watchers, &camera_720p(), ROOMY);
    assert_eq!(plan.encodings.len(), 2);
    assert_eq!(plan.codec_for("p2"), Some(RealtimeVideoCodec::H264));
    assert_eq!(plan.codec_for("p0"), Some(RealtimeVideoCodec::H265));

    // One hardware session left (a screen share holds the rest): one encoding all can take.
    let tight = VideoEncodeBudget { hardware_sessions: 1, ..ROOMY };
    let plan = plan_video_streams(&pc(), &watchers, &camera_720p(), tight);
    assert_eq!(plan.encodings.len(), 1);
    assert_eq!(plan.encodings[0].codec, RealtimeVideoCodec::H264);
    assert_eq!(plan.encodings[0].receivers.len(), 3);
}

#[test]
fn a_thin_uplink_also_counts_against_the_plan() {
    let mut people = vec![pc(), bare()];
    for (index, person) in people.iter_mut().enumerate() {
        person.capabilities_id = format!("p{index}");
    }
    // HEVC (1.75 Mbit/s) + H.264 (2.5 Mbit/s) do not fit 3 Mbit/s; H.264 alone does.
    let thin = VideoEncodeBudget { uplink_bps: 3_000_000, ..ROOMY };
    let plan = plan_video_streams(&pc(), &group_watchers(&people), &camera_720p(), thin);
    assert_eq!(plan.encodings.len(), 1);
    assert_eq!(plan.encodings[0].codec, RealtimeVideoCodec::H264);
}

#[test]
fn a_sender_without_any_encoder_serves_nobody_and_says_so() {
    let mut people = vec![laptop()];
    people[0].capabilities_id = "p0".to_string();
    let plan = plan_video_streams(&bare(), &group_watchers(&people), &camera_720p(), ROOMY);
    assert!(plan.encodings.is_empty());
    assert_eq!(plan.unserved, vec!["p0".to_string()]);
}

#[test]
fn the_passport_reads_back_from_json_with_defaults() {
    let json = r#"{"dialog_key":"d","codecs":[{"codec":"av1","payload_type":45,"decode":{"hardware":false}}],"max_datagram_bytes":1200,"sent_at_unix_ms":1}"#;
    let parsed: RealtimeMediaCapabilitiesV1 = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.power, RealtimePowerSource::Mains);
    assert_eq!(parsed.cpu_load, RealtimeCpuLoad::Medium);
    assert!(parsed.codecs[0].encode.is_none());
    assert_eq!(parsed.codecs[0].decode.as_ref().unwrap().max_width, 0);
}

/// Phase 4a: one encoding per group sender, the best one every watcher decodes.
#[test]
fn a_group_sender_picks_the_one_codec_every_watcher_decodes() {
    let shape = camera_720p();
    let (pc_watcher, laptop_two, bare) = (pc(), laptop(), bare());
    // The laptop to the PC and another laptop: everybody decodes AV1 in hardware.
    let watchers = vec![("pc".to_string(), &pc_watcher), ("l2".to_string(), &laptop_two)];
    assert_eq!(choose_group_video_codec(&laptop(), &watchers, &shape), RealtimeVideoCodec::Av1);
    // The PC (no AV1 encoder) to a laptop and a bare machine without an HEVC decoder: HEVC would
    // leave the bare one blind, so H.264 for all.
    let watchers = vec![("l".to_string(), &laptop_two), ("bare".to_string(), &bare)];
    assert_eq!(choose_group_video_codec(&pc(), &watchers, &shape), RealtimeVideoCodec::H264);
    // The PC to laptops only: HEVC in hardware.
    let watchers = vec![("l".to_string(), &laptop_two)];
    assert_eq!(choose_group_video_codec(&pc(), &watchers, &shape), RealtimeVideoCodec::H265);
    // Nobody watching yet: the floor.
    assert_eq!(choose_group_video_codec(&laptop(), &[], &shape), RealtimeVideoCodec::H264);
}
