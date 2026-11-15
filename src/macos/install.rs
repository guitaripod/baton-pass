use super::pasteboard::console_user;
use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

const LABEL: &str = "dev.batonpass.BatonPass";
const BINARY: &str = "/usr/local/bin/baton-pass";
const PLIST: &str = "/Library/LaunchDaemons/dev.batonpass.BatonPass.plist";
const LOG: &str = "/var/log/baton-pass.log";
const ROTATION: &str = "/etc/newsyslog.d/baton-pass.conf";
const KARABINER: &str =
    "/Library/Application Support/org.pqrs/Karabiner-DriverKit-VirtualHIDDevice/Applications/Karabiner-VirtualHIDDevice-Daemon.app";

/// `sudo baton-pass install`: copies this binary to /usr/local/bin and runs it as a root
/// LaunchDaemon (root is what Karabiner's virtual HID daemon requires), allows it through the
/// application firewall, and gives LinearMouse a profile that keeps its virtual mouse linear.
pub fn install() -> Result<()> {
    require_root()?;
    if !Path::new(KARABINER).exists() {
        bail!("Karabiner's virtual HID driver is not installed; install Karabiner-Elements (https://karabiner-elements.pqrs.org) first");
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    std::fs::create_dir_all("/usr/local/bin")?;
    std::fs::create_dir_all(crate::config::Config::dir())?;
    if executable != Path::new(BINARY) {
        let staged = format!("{BINARY}.new");
        std::fs::copy(&executable, &staged).with_context(|| format!("copying to {staged}"))?;
        std::fs::rename(&staged, BINARY)?;
    }
    run("/bin/chmod", &["755", BINARY])?;
    let firewall = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    run(firewall, &["--add", BINARY])?;
    run(firewall, &["--unblockapp", BINARY])?;
    linear_mouse_profile();

    std::fs::write(PLIST, plist())?;
    run("/usr/sbin/chown", &["root:wheel", PLIST])?;
    run("/bin/chmod", &["644", PLIST])?;
    std::fs::write(ROTATION, format!("{LOG} 644 3 1024 * J\n"))?;
    stop();
    run("/bin/launchctl", &["bootstrap", "system", PLIST])?;

    println!("installed: {BINARY} runs as the LaunchDaemon {LABEL}, logging to {LOG}");
    if !crate::config::Config::key_path().exists() {
        println!("next: run `sudo baton-pass pair` here and `baton-pass pair <code>` on the desktop");
    }
    Ok(())
}

/// `sudo baton-pass uninstall`: stops the daemon and removes what `install` put in place.
/// The key and config in /usr/local/etc/baton-pass stay unless `purge` is set.
pub fn uninstall(purge: bool) -> Result<()> {
    require_root()?;
    stop();
    let _ = Command::new("/usr/libexec/ApplicationFirewall/socketfilterfw").args(["--remove", BINARY]).output();
    for path in [PLIST, ROTATION, BINARY] {
        if Path::new(path).exists() {
            std::fs::remove_file(path).with_context(|| format!("removing {path}"))?;
        }
    }
    if purge {
        let dir = crate::config::Config::dir();
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
    }
    println!("uninstalled{}", if purge { ", key and config removed" } else { "; the key and config stay in /usr/local/etc/baton-pass" });
    Ok(())
}

fn require_root() -> Result<()> {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    if unsafe { geteuid() } != 0 {
        bail!("run this with sudo");
    }
    Ok(())
}

/// Boots out a running daemon and waits until launchd has let it go, so a bootstrap right after
/// does not race the old instance.
fn stop() {
    let target = format!("system/{LABEL}");
    let loaded = || Command::new("/bin/launchctl").args(["print", &target]).output().is_ok_and(|output| output.status.success());
    if !loaded() {
        return;
    }
    let _ = Command::new("/bin/launchctl").args(["bootout", &target]).output();
    for _ in 0..50 {
        if !loaded() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// LinearMouse resets every pointing device without a matching scheme to the system's
/// acceleration whenever the active device changes, so its config gets a scheme for Karabiner's
/// virtual pointer that turns acceleration off. Nothing happens when LinearMouse is not set up.
fn linear_mouse_profile() {
    let Some((_, user)) = console_user() else { return };
    let path = Path::new("/Users").join(&user).join(".config/linearmouse/linearmouse.json");
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(schemes) = config.as_object_mut().map(|object| object.entry("schemes").or_insert_with(|| serde_json::json!([]))) else { return };
    let Some(schemes) = schemes.as_array_mut() else { return };
    let ours = |scheme: &serde_json::Value| {
        let conditions = match scheme.get("if") {
            Some(serde_json::Value::Array(conditions)) => conditions.clone(),
            Some(condition) => vec![condition.clone()],
            None => Vec::new(),
        };
        conditions.iter().any(|condition| {
            condition.pointer("/device/productID").and_then(|id| id.as_str()).is_some_and(|id| id.eq_ignore_ascii_case("0x27da"))
        })
    };
    if schemes.iter().any(ours) {
        return;
    }
    schemes.push(serde_json::json!({
        "if": { "device": { "category": "mouse", "vendorID": "0x16c0", "productID": "0x27da" } },
        "pointer": { "disableAcceleration": true }
    }));
    if let Ok(text) = serde_json::to_string_pretty(&config)
        && std::fs::write(&path, text + "\n").is_ok()
    {
        println!("added a LinearMouse profile that keeps Baton Pass's virtual mouse free of acceleration");
    }
}

fn plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{BINARY}</string><string>run</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardErrorPath</key><string>{LOG}</string>
</dict>
</plist>
"#
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}
