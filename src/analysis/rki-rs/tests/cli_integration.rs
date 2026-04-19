use std::process::Command;

#[test]
fn test_binary_help_output() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success(), "Binary exited with error");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Rust Kimi CLI Agent"), "Help should contain program name");
    assert!(stdout.contains("--resume"), "Help should mention --resume");
    assert!(stdout.contains("--yolo"), "Help should mention --yolo");
    assert!(stdout.contains("--model"), "Help should mention --model");
    assert!(
        stdout.contains("--show-unified-events"),
        "Help should mention unified events dump"
    );
}

#[test]
fn test_binary_version_in_help() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("rki"), "Help should contain binary name");
}
