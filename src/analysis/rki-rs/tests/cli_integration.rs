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
    assert!(
        stdout.contains("Rust Kimi CLI Agent"),
        "Help should contain program name"
    );
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

#[test]
fn test_binary_version_flag() {
    let output = Command::new("cargo")
        .args(["run", "--", "--version"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success(), "--version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("rki"), "Version should contain binary name");
    assert!(
        stdout.split_whitespace().nth(1).unwrap().contains('.'),
        "Version should contain a semver: {stdout}"
    );
}

#[test]
fn test_binary_config_flag_in_help() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--config"), "Help should mention --config");
}

#[test]
fn test_binary_list_sessions_flag_in_help() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--list-sessions"), "Help should mention --list-sessions");
}

#[test]
fn test_binary_export_session_flag_in_help() {
    let output = Command::new("cargo")
        .args(["run", "--", "--help"])
        .current_dir(std::env::current_dir().unwrap())
        .output()
        .expect("Failed to execute binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--export-session"), "Help should mention --export-session");
}
