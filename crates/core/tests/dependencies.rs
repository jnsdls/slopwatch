use std::process::Command;

/// Core is framework-free: nothing it depends on, directly or not, is
/// another slopwatch crate or GPUI.
#[test]
fn core_depends_on_no_daemon_protocol_or_gpui_crate() {
    let output = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--offline",
            "--package",
            "slopwatch-core",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8(output.stdout).unwrap();
    let forbidden: Vec<&str> = tree
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| {
            *name != "slopwatch-core" && (name.starts_with("slopwatch-") || name.contains("gpui"))
        })
        .collect();
    assert!(forbidden.is_empty(), "core depends on {forbidden:?}");
}
