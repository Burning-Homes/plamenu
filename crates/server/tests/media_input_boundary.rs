//! Local-only regression fixtures for untrusted media demuxer boundaries.
mod common;

use plamenu::media_transcode::probe;

#[tokio::test]
async fn probing_an_upload_rejects_playlists_that_reference_another_local_file() {
    let temp = tempfile::tempdir().unwrap();
    let referenced = temp.path().join("not-the-upload.wav");
    assert!(
        std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "0.25",
                "-c:a",
                "pcm_s16le",
                "-y",
            ])
            .arg(&referenced)
            .status()
            .unwrap()
            .success()
    );
    let upload = temp.path().join("upload.orig");
    std::fs::write(&upload, "ffconcat version 1.0\nfile 'not-the-upload.wav'\n").unwrap();
    assert!(
        probe(&common::test_config(), &upload).await.is_err(),
        "reject an unsupported playlist before it can probe a different local file"
    );
}

#[tokio::test]
async fn probing_an_upload_makes_no_outbound_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let upload = temp.path().join("upload.orig");
    std::fs::write(&upload, format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXTINF:0.25,\nhttp://{address}/fixture.ts\n#EXT-X-ENDLIST\n",
    )).unwrap();
    let config = common::test_config();
    tokio::select! {
        accepted = listener.accept() => panic!("media probe attempted a network connection: {}", accepted.is_ok()),
        result = probe(&config, &upload) => assert!(result.is_err()),
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unlimited_probe_output_is_rejected_without_waiting_for_the_probe_timeout() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let probe_script = temp.path().join("probe-fixture");
    // exec keeps the noisy process at the child PID covered by kill_on_drop.
    std::fs::write(&probe_script, "#!/bin/sh\nexec yes x\n").unwrap();
    std::fs::set_permissions(&probe_script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = common::test_config();
    config.ffprobe_path = probe_script.to_str().unwrap().to_owned();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        probe(&config, &temp.path().join("ignored")),
    )
    .await
    .expect("bounded output must fail before the ordinary 30-second probe timeout");
    assert!(result.is_err());
}
