mod common;
use plamenu::{
    config::MusicConfig,
    media_limits::FileSizeLimit,
    music_renditions::{self, Format},
};
use std::path::Path;
fn wave(file: &Path) {
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=437:duration=2",
                "-c:a",
                "pcm_s16le",
                "-y"
            ])
            .arg(file)
            .status()
            .unwrap()
            .success()
    );
}
fn limit(bytes: u64) -> FileSizeLimit {
    FileSizeLimit {
        launcher: env!("CARGO_BIN_EXE_plamenu").into(),
        bytes,
    }
}
async fn real_format(name: &str, codec: &str, magic: &[u8]) {
    let root = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = root.path().join("store");
    std::fs::create_dir_all(&config.media_dir).unwrap();
    let source = root.path().join("source.wav");
    wave(&source);
    config.music = MusicConfig {
        max_bitrate: 250,
        user_max_bitrate: 250,
        ..MusicConfig::default()
    };
    let selection = music_renditions::selection(config.music, name, Some(512), None).unwrap();
    if selection.format == Format::Mp3 {
        assert_eq!(selection.bitrate, 224);
    } else {
        assert_eq!(selection.bitrate, 250);
    }
    let encoded = music_renditions::encode(&config, &source, selection, &limit(1024 * 1024))
        .await
        .unwrap();
    let bytes = std::fs::read(&encoded.file).unwrap();
    assert_eq!(
        encoded.selection.bitrate,
        if name == "ogg" {
            240
        } else {
            selection.bitrate
        }
    );
    if name == "aac" {
        assert_eq!(&bytes[4..8], magic);
    } else {
        assert!(bytes.starts_with(magic));
    }
    assert_eq!(encoded.bytes, u64::try_from(bytes.len()).unwrap());
    let probe = plamenu::media_transcode::probe(&config, &encoded.file)
        .await
        .unwrap();
    assert_eq!(probe.audio_codec.as_deref(), Some(codec));
    let channels = std::process::Command::new(&config.ffprobe_path)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=channels",
            "-of",
            "csv=p=0",
        ])
        .arg(&encoded.file)
        .output()
        .unwrap();
    assert!(channels.status.success());
    assert_eq!(String::from_utf8(channels.stdout).unwrap().trim(), "1");
    assert!(probe.duration.unwrap() > 1.8);
    assert_eq!(probe.video_codec, None);
    let decoded = std::process::Command::new(&config.ffmpeg_path)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(&encoded.file)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    let scratch = encoded.directory.path().to_owned();
    drop(encoded);
    assert!(!scratch.exists());
    assert!(source.exists());
}
#[tokio::test]
async fn real_mp3_rendition_uses_capped_standard_rate_and_cleans_private_scratch() {
    real_format("mp3", "mp3", b"ID3").await;
}
#[tokio::test]
async fn real_vorbis_rendition_has_ogg_container_and_playable_audio() {
    real_format("ogg", "vorbis", b"OggS").await;
}
#[tokio::test]
async fn vorbis_negotiates_source_rate_without_upmixing_or_exceeding_ceiling() {
    let root = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = root.path().join("store");
    std::fs::create_dir_all(&config.media_dir).unwrap();
    for (sample_rate, channels, requested, expected) in [
        (8000, 1, 192, Some(42)),
        (16000, 1, 192, Some(100)),
        (22050, 1, 192, Some(90)),
        (32000, 1, 192, Some(190)),
        (44100, 2, 192, Some(192)),
        (48000, 1, 32, Some(32)),
        (44100, 2, 32, None),
        (96000, 1, 192, None),
    ] {
        let source = root.path().join("source.wav");
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "fatal",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=437:duration=0.2",
                "-ar",
            ])
            .arg(sample_rate.to_string())
            .arg("-ac")
            .arg(channels.to_string())
            .args(["-c:a", "pcm_s16le", "-y"])
            .arg(&source)
            .output()
            .unwrap();
        assert!(output.status.success());
        let selection =
            music_renditions::selection(config.music, "ogg", Some(requested), None).unwrap();
        let result =
            music_renditions::encode(&config, &source, selection, &limit(1024 * 1024)).await;
        if let Some(expected) = expected {
            let encoded = result.unwrap();
            assert_eq!(encoded.selection.bitrate, expected);
            let output = std::process::Command::new(&config.ffprobe_path)
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "a:0",
                    "-show_entries",
                    "stream=sample_rate,channels",
                    "-of",
                    "json",
                ])
                .arg(&encoded.file)
                .output()
                .unwrap();
            assert!(output.status.success());
            let data: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(data["streams"][0]["sample_rate"], sample_rate.to_string());
            assert_eq!(data["streams"][0]["channels"], channels);
        } else {
            assert!(matches!(
                result,
                Err(plamenu::error::ApiError::Unprocessable(_))
            ));
        }
        assert_eq!(
            std::fs::read_dir(config.media_dir.join("tmp")).map_or(0, Iterator::count),
            0
        );
    }
}
#[tokio::test]
async fn real_flac_rendition_has_lossless_container_and_playable_audio() {
    real_format("flac", "flac", b"fLaC").await;
}
#[tokio::test]
async fn real_aac_rendition_has_mp4_container_and_playable_audio() {
    real_format("aac", "aac", b"ftyp").await;
}
#[tokio::test]
async fn real_opus_rendition_has_ogg_container_and_playable_audio() {
    real_format("opus", "opus", b"OggS").await;
}
#[tokio::test]
async fn rendition_kernel_file_ceiling_refuses_output_and_cleans_private_scratch() {
    let root = tempfile::tempdir().unwrap();
    let mut config = common::test_config();
    config.media_dir = root.path().join("store");
    std::fs::create_dir_all(&config.media_dir).unwrap();
    let source = root.path().join("source.wav");
    wave(&source);
    let selection = music_renditions::selection(config.music, "mp3", None, None).unwrap();
    assert!(
        music_renditions::encode(&config, &source, selection, &limit(32))
            .await
            .is_err()
    );
    assert!(source.exists());
    assert_eq!(
        std::fs::read_dir(config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn bitrate_authority_uses_permissions_and_applies_every_ceiling() {
    use plamenu_db::role::{Role, permission};
    let mut role = Role {
        id: 12,
        name: "Administrator".into(),
        color: String::new(),
        position: 100,
        permissions: permission::UPLOAD_CUSTOM_EMOJIS,
        highlighted: true,
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    let config = MusicConfig::default();
    for (permission, ceiling) in [
        (permission::UPLOAD_CUSTOM_EMOJIS, 192),
        (permission::MANAGE_REPORTS, 256),
        (permission::MANAGE_SETTINGS, 320),
        (permission::ADMINISTRATOR, 320),
    ] {
        role.permissions = permission;
        for format in ["mp3", "ogg", "flac", "aac", "opus"] {
            assert_eq!(
                music_renditions::selection(config, format, Some(512), Some(&role))
                    .unwrap()
                    .bitrate,
                ceiling
            );
        }
    }
    assert_eq!(
        music_renditions::selection(config, "opus", Some(512), None)
            .unwrap()
            .bitrate,
        192
    );
    role.name = "Ordinary member".into();
    assert_eq!(
        music_renditions::selection(config, "opus", None, Some(&role))
            .unwrap()
            .bitrate,
        config.default_bitrate
    );
    let restricted = MusicConfig {
        max_bitrate: 250,
        ..config
    };
    assert_eq!(
        music_renditions::selection(restricted, "mp3", Some(512), Some(&role))
            .unwrap()
            .bitrate,
        224
    );
    assert_eq!(
        music_renditions::selection(restricted, "opus", Some(512), Some(&role))
            .unwrap()
            .bitrate,
        250
    );
    assert_eq!(
        music_renditions::selection(config, "opus", Some(64), Some(&role))
            .unwrap()
            .bitrate,
        64
    );
    for bitrate in [0, 31, 513, u32::MAX] {
        assert!(music_renditions::selection(config, "opus", Some(bitrate), Some(&role)).is_err());
    }
}
