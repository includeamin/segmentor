//! End-to-end tests for the development packaging command.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn packages_fixture_into_separate_fragmented_tracks() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = root.join("target/package-integration-test");
    let repeated_output = root.join("target/package-integration-test-repeat");
    let _ = fs::remove_dir_all(&output);
    let _ = fs::remove_dir_all(&repeated_output);

    run_packager(&root, &output);
    run_packager(&root, &repeated_output);
    assert_top_level_box(&output.join("video-init.mp4"), *b"ftyp");
    assert_top_level_box(&output.join("audio-1-init.mp4"), *b"ftyp");
    for track in ["video", "audio-1"] {
        for segment in 0..3 {
            let name = format!("{track}-{segment}.m4s");
            assert_top_level_box(&output.join(&name), *b"moof");
            assert_eq!(
                fs::read(output.join(&name)).expect("first artifact should be readable"),
                fs::read(repeated_output.join(&name))
                    .expect("repeated artifact should be readable"),
                "{name} should be deterministic"
            );
        }
    }
    for name in ["video-init.mp4", "audio-1-init.mp4"] {
        assert_eq!(
            fs::read(output.join(name)).expect("first init should be readable"),
            fs::read(repeated_output.join(name)).expect("repeated init should be readable"),
            "{name} should be deterministic"
        );
    }

    fs::remove_dir_all(output).expect("test output should be removable");
    fs::remove_dir_all(repeated_output).expect("repeated output should be removable");
}

/// A source that is already encrypted can only be served with the keys the mapper gives, and the
/// development command has none: it must refuse, not write ciphertext as if it were clear.
#[test]
fn refuses_an_encrypted_source_instead_of_writing_ciphertext() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for name in [
        "h264-aac-cenc.mp4",
        "h264-aac-cbcs.mp4",
        "h264-aac-cenc-ffmpeg.mp4",
    ] {
        let output = root.join(format!("target/package-encrypted-source-{name}"));
        let _ = fs::remove_dir_all(&output);
        let result = Command::new(env!("CARGO_BIN_EXE_vodnik"))
            .args(["package", "--input"])
            .arg(root.join("tests/fixtures").join(name))
            .arg("--output")
            .arg(&output)
            .current_dir(&root)
            .output()
            .expect("application should start");
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{name} was packaged");
        assert!(
            stderr.contains("encrypted media is not supported"),
            "{name}: {stderr}"
        );
        assert!(
            !output.join("video-0.m4s").exists(),
            "{name}: no segment of an encrypted source is written"
        );
        let _ = fs::remove_dir_all(&output);
    }
}

fn run_packager(root: &Path, output: &Path) {
    let result = Command::new(env!("CARGO_BIN_EXE_vodnik"))
        .args([
            "package",
            "--input",
            "tests/fixtures/h264-aac.mp4",
            "--output",
        ])
        .arg(output)
        .current_dir(root)
        .output()
        .expect("application should start");

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
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
