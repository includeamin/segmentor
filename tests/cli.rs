//! Integration tests for the application executable.

use std::process::Command;

#[test]
fn prints_the_application_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_vodnik"))
        .output()
        .expect("application should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"vodnik\n");
}
