use std::process::Stdio;
use tokio::process::Command;

const MATCHING: &str = r#"{"VendorID":0x16c0,"ProductID":0x27da}"#;
const ACCELERATION_OFF: &str = r#"{"HIDMouseAcceleration":-65536}"#;

/// Turns macOS pointer acceleration off for Karabiner's virtual pointer, so one count moves the
/// cursor a fixed distance and the desktop's own (already accelerated) motion carries over 1:1.
/// LinearMouse re-applies its own settings whenever the active pointing device changes, so when
/// it runs it is left in charge (install-mac.sh gives it a profile for this device) and Baton Pass
/// only steps in on Macs without it.
pub async fn ensure_linear_pointer() {
    if running("LinearMouse").await {
        return;
    }
    let result = Command::new("/usr/bin/hidutil")
        .args(["property", "--matching", MATCHING, "--set", ACCELERATION_OFF])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await;
    match result {
        Ok(output) if !output.status.success() => {
            tracing::warn!("hidutil failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Err(error) => tracing::warn!("running hidutil: {error}"),
        Ok(_) => {}
    }
}

async fn running(process: &str) -> bool {
    Command::new("/usr/bin/pgrep")
        .args(["-x", process])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}
