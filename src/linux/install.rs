use super::control::OBJECT_PATH;
use super::keymap::Hotkey;
use crate::config::Config;
use crate::crypto;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const APP_ID: &str = super::control::BUS_NAME;
const SCRIPT_ID: &str = "batonpass";

pub async fn install(peers: Vec<String>) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let executable = settle_executable(&home)?;

    let config_path = Config::path();
    if !config_path.exists() {
        if peers.is_empty() {
            bail!("first install needs the Mac's address: baton-pass install --peer <lan-ip> [--peer <tailnet-name>]");
        }
        write(&config_path, &default_config(&peers))?;
        println!("wrote {}", config_path.display());
    } else if !peers.is_empty() {
        let existing = std::fs::read_to_string(&config_path)?;
        write(&config_path, &with_peers(&existing, &peers))?;
        println!("set peers in {}", config_path.display());
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
    claim_shortcut(&hotkey).await?;

    let unit = home.join(".config/systemd/user/baton-pass.service");
    write(&unit, &systemd_unit(&executable))?;
    run("systemctl", &["--user", "daemon-reload"]).await?;
    run("systemctl", &["--user", "enable", "baton-pass.service"]).await?;
    run("systemctl", &["--user", "restart", "baton-pass.service"]).await?;

    println!("installed: baton-pass.service running, hotkey {} registered with KWin", hotkey.qt_name());
    println!("key: {}", Config::key_path().display());
    Ok(())
}

/// The binary the service will run. One run from a download folder copies itself to
/// ~/.local/bin first, so the service does not depend on where the archive was unpacked; one
/// already in ~/.cargo/bin, ~/.local/bin or a system folder stays where it is.
fn settle_executable(home: &Path) -> Result<PathBuf> {
    let current = std::env::current_exe()?.canonicalize()?;
    if is_settled(&current, home) {
        return Ok(current);
    }
    let local = home.join(".local/bin");
    std::fs::create_dir_all(&local)?;
    let target = local.join("baton-pass");
    let staged = local.join(".baton-pass.new");
    std::fs::copy(&current, &staged).with_context(|| format!("copying to {}", staged.display()))?;
    std::fs::rename(&staged, &target)?;
    println!("copied to {}", target.display());
    Ok(target)
}

fn is_settled(executable: &Path, home: &Path) -> bool {
    [home.join(".cargo/bin"), home.join(".local/bin"), PathBuf::from("/usr")].iter().any(|dir| executable.starts_with(dir))
}

/// Stops the service and removes what `install` put in place: the unit, the KWin script and
/// its shortcut, and the desktop entry. The config and key stay unless `purge` is set; the
/// binary itself is left for whatever installed it (`cargo uninstall baton-pass`).
pub async fn uninstall(purge: bool) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let _ = run("systemctl", &["--user", "disable", "--now", "baton-pass.service"]).await;
    remove(&home.join(".config/systemd/user/baton-pass.service"))?;
    let _ = run("systemctl", &["--user", "daemon-reload"]).await;
    if let Ok(connection) = zbus::Connection::session().await {
        let scripting = KdeService { name: "org.kde.KWin", path: "/Scripting", interface: "org.kde.kwin.Scripting" };
        let _ = scripting.call(&connection, "unloadScript", &(SCRIPT_ID,)).await;
        let accel = KdeService { name: "org.kde.kglobalaccel", path: "/kglobalaccel", interface: "org.kde.KGlobalAccel" };
        let _ = accel.call(&connection, "unregister", &("kwin", "BatonPassToggle")).await;
    }
    let _ = run("kwriteconfig6", &["--file", "kwinrc", "--group", "Plugins", "--key", &format!("{SCRIPT_ID}Enabled"), "--delete"]).await;
    remove(&home.join(".local/share/kwin/scripts").join(SCRIPT_ID))?;
    remove(&home.join(".local/share/applications").join(format!("{APP_ID}.desktop")))?;
    if purge {
        for dir in [Config::dir(), crate::config::xdg_dir("XDG_STATE_HOME", ".local/state"), crate::config::xdg_dir("XDG_CACHE_HOME", ".cache")] {
            remove(&dir)?;
        }
    }
    println!("uninstalled{}", if purge { ", config and key removed" } else { "; the config and key stay in ~/.config/baton-pass" });
    println!("remove the binary with `cargo uninstall baton-pass` or by deleting it");
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    let result = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    match result {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error).with_context(|| format!("removing {}", path.display())),
        _ => Ok(()),
    }
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
    let scripting = KdeService { name: "org.kde.KWin", path: "/Scripting", interface: "org.kde.kwin.Scripting" };
    let _ = scripting.call(&connection, "unloadScript", &(SCRIPT_ID,)).await;
    let path_text = main_script.to_string_lossy().to_string();
    scripting.call(&connection, "loadScript", &(path_text.as_str(), SCRIPT_ID)).await.context("loading the KWin script")?;
    scripting.call(&connection, "start", &()).await.context("starting KWin scripts")?;
    let loaded: bool = scripting.call(&connection, "isScriptLoaded", &(SCRIPT_ID,)).await?.body().deserialize()?;
    if !loaded {
        bail!("KWin did not load the baton-pass script");
    }
    Ok(())
}

/// A KDE D-Bus object to call methods on.
struct KdeService {
    name: &'static str,
    path: &'static str,
    interface: &'static str,
}

impl KdeService {
    async fn call<B>(&self, connection: &zbus::Connection, method: &str, body: &B) -> zbus::Result<zbus::message::Message>
    where
        B: serde::Serialize + zbus::DynamicType,
    {
        connection.call_method(Some(self.name), self.path, Some(self.interface), method, body).await
    }
}

fn peers_line(peers: &[String]) -> String {
    format!("peers = [{}]", peers.iter().map(|peer| format!("{peer:?}")).collect::<Vec<_>>().join(", "))
}

/// Replaces the `peers` line of an existing config, keeping everything else as the user wrote it.
fn with_peers(config: &str, peers: &[String]) -> String {
    let line = peers_line(peers);
    let mut replaced = false;
    let mut lines: Vec<String> = config
        .lines()
        .map(|existing| {
            let key = existing.trim_start();
            if !replaced && (key.starts_with("peers ") || key.starts_with("peers=")) {
                replaced = true;
                line.clone()
            } else {
                existing.to_string()
            }
        })
        .collect();
    if !replaced {
        lines.insert(0, line);
    }
    lines.join("\n") + "\n"
}

/// Makes the hotkey Baton Pass's alone: kglobalaccel hands a key to the first action that has it,
/// and Plasma ships Meta+Esc as System Monitor's launcher, so other owners are unbound.
async fn claim_shortcut(hotkey: &Hotkey) -> Result<()> {
    let Some(combination) = hotkey.qt_combination() else { return Ok(()) };
    let connection = zbus::Connection::session().await?;
    let accel = KdeService { name: "org.kde.kglobalaccel", path: "/kglobalaccel", interface: "org.kde.KGlobalAccel" };
    type Owner = (String, String, String, String, String, String, Vec<i32>, Vec<i32>);
    let owners: Vec<Owner> = accel.call(&connection, "getGlobalShortcutsByKey", &(combination,)).await?.body().deserialize()?;
    for (unique, friendly, component, component_friendly, ..) in owners {
        if component == "kwin" && unique == "BatonPassToggle" {
            continue;
        }
        let action = vec![component.clone(), unique.clone(), component_friendly, friendly.clone()];
        accel
            .call(&connection, "setShortcutKeys", &(action, Vec::<(Vec<i32>,)>::new(), 4u32))
            .await
            .with_context(|| format!("unbinding {} from {friendly}", hotkey.qt_name()))?;
        println!("took {} from \"{friendly}\" ({component})", hotkey.qt_name());
    }
    let ours = vec!["kwin".to_string(), "BatonPassToggle".into(), "KWin".into(), "Baton Pass: Carry Mouse and Keyboard to the Mac".into()];
    accel
        .call(&connection, "setShortcutKeys", &(ours, vec![(vec![combination, 0, 0, 0],)], 4u32))
        .await
        .context("binding the hotkey")?;
    Ok(())
}

fn default_config(peers: &[String]) -> String {
    let peers = peers_line(peers);
    format!(
        r#"# The Mac, tried in order until one answers. Put the wired LAN address first.
{peers}
# Which edge of this desktop the Mac sits beyond: left, right, top or bottom.
side = "left"
# Here it sends the mouse and keyboard to the Mac; on the Mac it brings them back.
hotkey = "Meta+Escape"
# Also switch by pushing the pointer off that edge (and back off the Mac's opposite edge).
edge = true
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
        "[Desktop Entry]\nType=Application\nName=Baton Pass\nComment=Carry the mouse and keyboard over to the Mac\nExec={} toggle\nIcon=input-mouse\nNoDisplay=true\n",
        executable.display()
    )
}

fn systemd_unit(executable: &Path) -> String {
    format!(
        "[Unit]\nDescription=Baton Pass: carry this desktop's mouse and keyboard over to the Mac\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nExecStart={} run\nRestart=on-failure\nRestartSec=2\n\n[Install]\nWantedBy=graphical-session.target\n",
        executable.display()
    )
}

const METADATA: &str = r#"{
    "KPlugin": {
        "Id": "batonpass",
        "Name": "Baton Pass",
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
    let shortcut = shortcut.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"registerShortcut("BatonPassToggle", "Baton Pass: Carry Mouse and Keyboard to the Mac", "{shortcut}", function () {{
    const position = workspace.cursorPos;
    callDBus("{APP_ID}", "{OBJECT_PATH}", "{APP_ID}", "Toggle", String(position.x), String(position.y));
}});
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_line_is_replaced_in_place() {
        let config = "# comment\npeers = [\"10.0.0.1\"]\nside = \"left\"\n";
        let updated = with_peers(config, &["10.0.0.2".into(), "mac".into()]);
        assert_eq!(updated, "# comment\npeers = [\"10.0.0.2\", \"mac\"]\nside = \"left\"\n");
        assert!(with_peers("side = \"left\"\n", &["a".into()]).starts_with("peers = [\"a\"]\n"));
    }

    #[test]
    fn downloaded_binaries_move_and_installed_ones_stay() {
        let home = Path::new("/home/someone");
        assert!(is_settled(Path::new("/home/someone/.cargo/bin/baton-pass"), home));
        assert!(is_settled(Path::new("/home/someone/.local/bin/baton-pass"), home));
        assert!(is_settled(Path::new("/usr/bin/baton-pass"), home));
        assert!(!is_settled(Path::new("/home/someone/Downloads/baton-pass-0.1.0-alpha.1/baton-pass"), home));
    }

    #[test]
    fn shortcut_is_escaped_for_javascript() {
        assert!(kwin_script("Meta+\\").contains("\"Meta+\\\\\""));
        assert!(kwin_script("Meta+Esc").contains("\"/dev/batonpass/BatonPass\""));
    }
}
