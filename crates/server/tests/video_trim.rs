//! Real decoder proof: an input seek inside a long GOP must not expose its
//! preceding keyframe, and explicit reprocessing must not merely remux H.264.
mod common;
use plamenu::{
    media_transcode::{self, TranscodeParams},
    video_clip::Window,
};
use std::path::Path;

fn source(path: &Path) {
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:size=64x64:rate=8:duration=2",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:size=64x64:rate=8:duration=2",
                "-f",
                "lavfi",
                "-i",
                "color=c=green:size=64x64:rate=8:duration=2",
                "-filter_complex",
                "[0:v][1:v][2:v]concat=n=3:v=1:a=0[v]",
                "-map",
                "[v]",
                "-c:v",
                "libx264",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "480",
                "-keyint_min",
                "480",
                "-sc_threshold",
                "0",
                "-y"
            ])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

fn decoded_pixels(path: &Path) -> Vec<u8> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args([
            "-vf",
            "scale=1:1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}

fn h264_packets(path: &Path) -> Vec<u8> {
    let output = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-c", "copy", "-f", "h264", "pipe:1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    output.stdout
}

#[tokio::test]
async fn accurate_trim_decodes_only_selected_frames_and_retains_source_on_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("long-gop.mp4");
    source(&input);
    let original = std::fs::read(&input).unwrap();
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let mut params = TranscodeParams {
        window: Some(Window::new(2250, 3250).unwrap()),
        crf: Some(18),
        ..Default::default()
    };
    let clipped = media_transcode::process_native_video_file(&config, &input, &params, 0.0)
        .await
        .unwrap();
    assert!((clipped.duration.unwrap() - 1.0).abs() < 0.05);
    let pixels = decoded_pixels(&clipped.file_path);
    assert_eq!(pixels.len(), 8 * 3);
    assert!(
        pixels
            .as_chunks::<3>()
            .0
            .iter()
            .all(|pixel| pixel[0] < 30 && pixel[1] < 30 && pixel[2] > 200),
        "every selected frame must be blue, never the preceding red keyframe or following green footage: {pixels:?}"
    );
    assert_eq!(std::fs::read(&input).unwrap(), original);
    params.window = Some(Window::new(6250, 7250).unwrap());
    assert!(
        media_transcode::process_native_video_file(&config, &input, &params, 0.0)
            .await
            .is_err()
    );
    assert!(
        media_transcode::process_av_file(&config, &input, &params)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&input).unwrap(), original);
}

#[tokio::test]
async fn audio_source_trim_uses_the_selected_sound_and_remains_a_native_waveform_video() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("tones.m4a");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=220:sample_rate=48000:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:sample_rate=48000:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=2",
                "-filter_complex",
                "[0:a][1:a][2:a]concat=n=3:v=0:a=1[a]",
                "-map",
                "[a]",
                "-c:a",
                "aac",
                "-b:a",
                "128k",
                "-y"
            ])
            .arg(&input)
            .status()
            .unwrap()
            .success()
    );
    let original = std::fs::read(&input).unwrap();
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let params = TranscodeParams {
        window: Some(Window::new(2250, 3250).unwrap()),
        ..Default::default()
    };
    let output = media_transcode::process_native_video_file(&config, &input, &params, 0.0)
        .await
        .unwrap();
    assert_eq!(output.content_type, "video/mp4");
    assert_eq!((output.width, output.height), (Some(640), Some(360)));
    assert!((output.duration.unwrap() - 1.0).abs() < 0.05);
    let sound = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(&output.file_path)
        .args([
            "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "s16le", "pipe:1",
        ])
        .output()
        .unwrap();
    assert!(sound.status.success());
    let samples: Vec<_> = sound
        .stdout
        .as_chunks::<2>()
        .0
        .iter()
        .map(|v| i16::from_le_bytes(*v))
        .collect();
    assert!(samples.len() >= 42000);
    let crossings = samples[6000..42000]
        .windows(2)
        .filter(|v| (v[0] < 0) != (v[1] < 0))
        .count();
    assert!(
        (1100..1450).contains(&crossings),
        "selected 880 Hz section, not preceding 220/following 440 Hz audio: {crossings}"
    );
    assert_eq!(std::fs::read(input).unwrap(), original);
}

#[tokio::test]
async fn explicit_reprocess_reencodes_an_otherwise_copyable_film() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("source.mp4");
    source(&input);
    let mut config = common::test_config();
    config.media_dir = temp.path().join("media");
    let packets = h264_packets(&input);
    let mut params = TranscodeParams {
        crf: Some(42),
        ..Default::default()
    };
    let copied = media_transcode::process_native_video_file(&config, &input, &params, 0.0)
        .await
        .unwrap();
    assert_eq!(h264_packets(&copied.file_path), packets);
    params.force_reencode = true;
    let processed = media_transcode::process_native_video_file(&config, &input, &params, 0.0)
        .await
        .unwrap();
    assert_ne!(h264_packets(&processed.file_path), packets);
    let hls = media_transcode::package_native_hls(&config, &processed, "editedfilm", &params)
        .await
        .unwrap();
    let durations: Vec<f64> = hls[0]
        .playlist
        .lines()
        .filter_map(|line| {
            line.strip_prefix("#EXTINF:")?
                .trim_end_matches(',')
                .parse()
                .ok()
        })
        .collect();
    assert_eq!(durations.len(), 2);
    assert!(
        durations.iter().all(|duration| *duration <= 4.001),
        "copy-top must not inherit the retained source's long GOP: {durations:?}"
    );
    assert!((processed.duration.unwrap() - 6.0).abs() < 0.05);
    assert_eq!(decoded_pixels(&processed.file_path).len(), 48 * 3);
    assert_eq!(h264_packets(&input), packets);
}
