use super::keymap::Hotkey;
use crate::config::Config;
use crate::crypto;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const APP_ID: &str = "dev.ferry.Ferry";
const SCRIPT_ID: &str = "ferry";

pub async fn install(peers: Vec<String>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let executable = std::env::current_exe()?.canonicalize()?;

    let config_path = Config::path();
    if !config_path.exists() {
        if peers.is_empty() {
            bail!("first install needs the Mac's address: ferry install --peer <lan-ip> [--peer <tailnet-name>]");
        }
        write(&config_path, &default_config(&peers))?;
        println!("wrote {}", config_path.display());
    }
    let config = Config::load()?;
    let hotkey = Hotkey::parse(&config.hotkey)?;
    crypto::ensure_key(&Config::key_path())?;

    let applications = home.join(".local/share/applications");
    write(&applications.join(format!("{APP_ID}.desktop")), &desktop_entry(&executable))?;

    let script_dir = home.join(".local/share/kwin/scripts").join(SCRIPT_ID);
    write(&script_dir.join("metadata.json"), METADATA)?;
    let main_script = script_dir.join("contents/code/main.js");
    write(&main_script, &kwin_script(hotkey.qt_name()))?;
    load_kwin_script(&main_script).await?;

    let unit = home.join(".config/systemd/user/ferry.service");
    write(&unit, &systemd_unit(&executable))?;
    run("systemctl", &["--user", "daemon-reload"]).await?;
    run("systemctl", &["--user", "enable", "ferry.service"]).await?;
    run("systemctl", &["--user", "restart", "ferry.service"]).await?;

    println!("installed: ferry.service running, hotkey {} registered with KWin", hotkey.qt_name());
    println!("key: {}", Config::key_path().display());
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

async fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status().await.with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

/// Enables the KWin script persistently and (re)loads it in the running session.
async fn load_kwin_script(main_script: &Path) -> Result<()> {
    run("kwriteconfig6", &["--file", "kwinrc", "--group", "Plugins", "--key", &format!("{SCRIPT_ID}Enabled"), "true"]).await?;
    let connection = zbus::Connection::session().await?;
    let scripting = |method: &'static str| (Some("org.kde.KWin"), "/Scripting", Some("org.kde.kwin.Scripting"), method);
    let (destination, path, interface, method) = scripting("unloadScript");
    let _ = connection.call_method(destination, path, interface, method, &(SCRIPT_ID,)).await;
    let (destination, path, interface, method) = scripting("loadScript");
    let path_text = main_script.to_string_lossy().to_string();
    connection
        .call_method(destination, path, interface, method, &(path_text.as_str(), SCRIPT_ID))
        .await
        .context("loading the KWin script")?;
    let (destination, path, interface, method) = scripting("start");
    connection.call_method(destination, path, interface, method, &()).await.context("starting KWin scripts")?;
    let (destination, path, interface, method) = scripting("isScriptLoaded");
    let loaded: bool = connection
        .call_method(destination, path, interface, method, &(SCRIPT_ID,))
        .await?
        .body()
        .deserialize()?;
    if !loaded {
        bail!("KWin did not load the ferry script");
    }
    Ok(())
}

fn default_config(peers: &[String]) -> String {
    let peers = peers.iter().map(|peer| format!("{peer:?}")).collect::<Vec<_>>().join(", ");
    format!(
        r#"# The Mac, tried in order until one answers. Put the wired LAN address first.
peers = [{peers}]
# Which edge of this desktop the Mac sits beyond: left, right, top or bottom.
side = "left"
# Here it sends the mouse and keyboard to the Mac; on the Mac it brings them back.
hotkey = "Meta+Escape"
# Mac points travelled per pixel moved here.
speed = 1.0
# This desktop's natural-scrolling setting, so the wheel turns the same way on the Mac.
natural_scroll = true
# Send Alt as Command and Meta as Option instead of Meta as Command.
swap_alt_meta = false
# Carry text and PNG clipboards across on every switch.
clipboard = true
"#
    )
}

fn desktop_entry(executable: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Ferry\nComment=Carry the mouse and keyboard over to the Mac\nExec={} toggle\nIcon=input-mouse\nNoDisplay=true\n",
        executable.display()
    )
}

fn systemd_unit(executable: &Path) -> String {
    format!(
        "[Unit]\nDescription=Ferry: carry this desktop's mouse and keyboard over to the Mac\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nExecStart={} run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=graphical-session.target\n",
        executable.display()
    )
}

const METADATA: &str = r#"{
    "KPlugin": {
        "Id": "ferry",
        "Name": "Ferry",
        "Description": "Hotkey that carries the mouse and keyboard over to the Mac",
        "License": "GPL-3.0-or-later",
        "EnabledByDefault": true
    },
    "X-Plasma-API": "javascript",
    "X-Plasma-MainScript": "code/main.js",
    "KPackageStructure": "KWin/Script"
}
"#;

fn kwin_script(shortcut: &str) -> String {
    format!(
        r#"registerShortcut("FerryToggle", "Ferry: Carry Mouse and Keyboard to the Mac", "{shortcut}", function () {{
    const position = workspace.cursorPos;
    callDBus("{APP_ID}", "/dev/ferry/Ferry", "{APP_ID}", "Toggle", String(position.x), String(position.y));
}});
"#
    )
}
