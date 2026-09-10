//! End-to-end tests for the development packaging command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn packages_fixture_into_separate_fragmented_tracks() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = root.join("target/package-integration-test");
    let _ = fs::remove_dir_all(&output);

    let result = Command::new(env!("CARGO_BIN_EXE_vod-module-rs"))
        .args([
            "package",
            "--input",
            "tests/fixtures/h264-aac.mp4",
            "--output",
        ])
        .arg(&output)
        .current_dir(&root)
        .output()
        .expect("application should start");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_top_level_box(&output.join("video-init.mp4"), *b"ftyp");
    assert_top_level_box(&output.join("audio-init.mp4"), *b"ftyp");
    for track in ["video", "audio"] {
        for segment in 0..3 {
            assert_top_level_box(&output.join(format!("{track}-{segment}.m4s")), *b"moof");
        }
    }

    fs::remove_dir_all(output).expect("test output should be removable");
}

fn assert_top_level_box(path: &Path, expected_type: [u8; 4]) {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert!(
        bytes.len() >= 8,
        "{} is shorter than an MP4 box header",
        path.display()
    );
    assert_eq!(
        &bytes[4..8],
        &expected_type,
        "unexpected first box in {}",
        path.display()
    );
}
