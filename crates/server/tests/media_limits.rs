//! Real kernel/FFmpeg checks, without a database or external limit helper.
#![cfg(unix)]
mod common;

use plamenu::media_limits::FileSizeLimit;
use plamenu::media_transcode::{TranscodeParams, process_av_file};
use rustix::process::{Resource, getrlimit};
use std::process::Stdio;

fn bounded_ffmpeg(limit: u64) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_plamenu"));
    command.args([
        "__media-exec",
        "--max-file-bytes",
        &limit.to_string(),
        "--parent-pid",
        &std::process::id().to_string(),
        "--ffmpeg",
        "ffmpeg",
        "--",
        "-nostdin",
        "-v",
        "error",
    ]);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

#[tokio::test]
async fn encoder_file_limit_is_kernel_enforced_and_does_not_change_parent() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("oversize.raw");
    let before = getrlimit(Resource::Fsize);
    let core_before = getrlimit(Resource::Core);
    let mut command = bounded_ffmpeg(65_536);
    command
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30",
            "-t",
            "3",
            "-f",
            "rawvideo",
            "-y",
        ])
        .arg(&output);
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        !result.status.success(),
        "an oversized encoder must fail, not publish a truncated film"
    );
    assert_eq!(std::fs::metadata(&output).unwrap().len(), 65_536);
    let after = getrlimit(Resource::Fsize);
    assert_eq!(before.current, after.current);
    assert_eq!(before.maximum, after.maximum);
    assert_eq!(core_before.current, getrlimit(Resource::Core).current);
    assert_eq!(core_before.maximum, getrlimit(Resource::Core).maximum);
    std::fs::write(temp.path().join("parent-can-still-write"), vec![0; 131_072]).unwrap();
}

#[tokio::test]
async fn bounded_encoder_accepts_small_output_without_config_or_database() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("small.raw");
    let mut command = bounded_ffmpeg(8192);
    command
        .current_dir(temp.path())
        .args([
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=8000:cl=mono",
            "-t",
            "0.25",
            "-f",
            "s16le",
            "-y",
        ])
        .arg(&output);
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::metadata(output).unwrap().len(), 4000);
}

#[tokio::test]
async fn bounded_pipeline_preserves_source_and_removes_failed_scratch() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.mp4");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x64:rate=10",
                "-t",
                "1",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
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
    let mut params = TranscodeParams {
        file_size_limit: Some(FileSizeLimit {
            launcher: env!("CARGO_BIN_EXE_plamenu").into(),
            bytes: 1,
        }),
        ..Default::default()
    };
    assert!(process_av_file(&config, &input, &params).await.is_err());
    assert_eq!(std::fs::read(&input).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
    params.file_size_limit.as_mut().unwrap().bytes = 1024 * 1024;
    let processed = process_av_file(&config, &input, &params).await.unwrap();
    assert!((processed.duration.unwrap() - 1.0).abs() < 0.1);
    assert!(processed.file_size > 0 && processed.file_size < 1024 * 1024);
    assert!(processed.small.is_some());
    drop(processed);
    assert_eq!(std::fs::read(&input).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(config.media_dir.join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_the_launcher_kills_the_same_encoder_process() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("running.raw");
    let mut command = bounded_ffmpeg(100 * 1024 * 1024);
    command
        .stderr(Stdio::null())
        .args([
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x64:rate=10",
            "-t",
            "300",
            "-f",
            "rawvideo",
            "-y",
        ])
        .arg(&output);
    let child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    let process_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if std::fs::read_link(process_dir.join("exe"))
                .is_ok_and(|p| p.file_name().is_some_and(|n| n == "ffmpeg"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("launcher must exec FFmpeg without a grandchild");
    drop(child);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while process_dir.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropped encoder must be killed and reaped");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn encoder_cannot_outlive_a_crashed_parent() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let temp = tempfile::tempdir().unwrap();
    // A disposable parent keeps stdin open while its real FFmpeg child runs.
    // All variable paths are positional shell arguments, never interpolated.
    let mut parent = tokio::process::Command::new("sh").arg("-c").arg(
        r#""$1" __media-exec --max-file-bytes 104857600 --parent-pid "$$" --ffmpeg ffmpeg -- -nostdin -v error -re -f lavfi -i testsrc2=size=64x64:rate=10 -t 300 -f rawvideo -y "$2" &
echo "$!"
read hold
wait"#
    ).arg("encoder-parent-fixture").arg(env!("CARGO_BIN_EXE_plamenu"))
        .arg(temp.path().join("running.raw")).stdin(Stdio::piped()).stdout(Stdio::piped())
        .stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
    let mut pid_line = String::new();
    BufReader::new(parent.stdout.take().unwrap())
        .read_line(&mut pid_line)
        .await
        .unwrap();
    let pid: u32 = pid_line.trim().parse().unwrap();
    let process_dir = std::path::PathBuf::from(format!("/proc/{pid}"));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if std::fs::read_link(process_dir.join("exe"))
                .is_ok_and(|p| p.file_name().is_some_and(|n| n == "ffmpeg"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    parent.kill().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            // The orphan may briefly be a zombie awaiting the host's init;
            // it must have no live executable after the parent's SIGKILL.
            if std::fs::read_link(process_dir.join("exe")).is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("parent death must kill the actual encoder");
}
