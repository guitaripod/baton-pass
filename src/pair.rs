//! Pairing: puts one fresh key on both machines from a short code read off the Mac. The code
//! runs through SPAKE2, so what crosses the network lets nobody test guesses offline, and a
//! wrong guess ends the pairing window on the Mac.

use crate::crypto::{self, Cipher};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const PORT: u16 = 47475;
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME: usize = 1024;
const DESKTOP: &[u8] = b"baton-pass desktop";
const MAC: &[u8] = b"baton-pass mac";
const CONFIRMATION: &[u8] = b"baton-pass paired";

#[cfg(any(target_os = "macos", test))]
/// A fresh eight-digit code, shown as `1234-5678`.
pub fn new_code() -> String {
    let digits = format!("{:08}", crypto::random_u64() % 100_000_000);
    format!("{}-{}", &digits[..4], &digits[4..])
}

/// The digits of a code as typed: spaces and dashes are ignored.
pub fn normalize(code: &str) -> Result<String> {
    let digits: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
    if digits.len() != 8 || !digits.chars().all(|c| c.is_ascii_digit()) {
        bail!("a pairing code is eight digits, like 1234-5678");
    }
    Ok(digits)
}

#[cfg(any(target_os = "macos", test))]
/// The Mac's half: answers one desktop on `stream`, takes the key it sends if the desktop
/// proved the same code, and writes it to `key_path`.
pub async fn host(stream: &mut TcpStream, code: &str, key_path: &Path) -> Result<()> {
    let (spake, outbound) = Spake2::<Ed25519Group>::start_b(&Password::new(normalize(code)?), &Identity::new(DESKTOP), &Identity::new(MAC));
    let inbound = read_frame(stream).await?;
    write_frame(stream, &outbound).await?;
    let cipher = session_cipher(spake.finish(&inbound).map_err(|error| anyhow::anyhow!("pairing handshake: {error:?}"))?);
    let sealed = read_frame(stream).await?;
    let key: [u8; 32] = cipher
        .open(&sealed)
        .context("the desktop typed a different code")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the desktop sent a malformed key"))?;
    crypto::write_key(key_path, &key)?;
    write_frame(stream, &cipher.seal(CONFIRMATION)).await?;
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
/// The desktop's half: proves `code` to the Mac at `address`, sends it a fresh key, and
/// writes the key to `key_path` once the Mac confirms it took it.
pub async fn join(address: &str, code: &str, key_path: &Path) -> Result<()> {
    let password = Password::new(normalize(code)?);
    let mut stream = tokio::time::timeout(STEP_TIMEOUT, TcpStream::connect(address))
        .await
        .context("the Mac did not answer")?
        .with_context(|| format!("connecting to {address} (is `sudo baton-pass pair` running on the Mac?)"))?;
    let (spake, outbound) = Spake2::<Ed25519Group>::start_a(&password, &Identity::new(DESKTOP), &Identity::new(MAC));
    write_frame(&mut stream, &outbound).await?;
    let inbound = read_frame(&mut stream).await?;
    let cipher = session_cipher(spake.finish(&inbound).map_err(|error| anyhow::anyhow!("pairing handshake: {error:?}"))?);
    let key = crypto::random_key();
    write_frame(&mut stream, &cipher.seal(&key)).await?;
    let confirmation = read_frame(&mut stream).await.context("the Mac did not accept the code")?;
    if cipher.open(&confirmation).as_deref() != Some(CONFIRMATION) {
        bail!("the Mac did not confirm the pairing");
    }
    crypto::write_key(key_path, &key)
}

/// `sudo baton-pass pair` on the Mac: shows a code, waits three minutes for the desktop to prove
/// it, then restarts the daemon on the new key.
#[cfg(target_os = "macos")]
pub async fn on_mac() -> Result<()> {
    let key_path = crate::config::Config::key_path();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT)).await.with_context(|| format!("listening on port {PORT}"))?;
    let code = new_code();
    println!("Pairing code: {code}");
    println!("On the desktop, run:  baton-pass pair {code}");
    let (mut stream, from) = tokio::time::timeout(Duration::from_secs(180), listener.accept())
        .await
        .context("no desktop paired within three minutes")??;
    host(&mut stream, &code, &key_path).await.with_context(|| format!("pairing with {from}"))?;
    let restarted = tokio::process::Command::new("/bin/launchctl").args(["kickstart", "-k", "system/dev.batonpass.BatonPass"]).status().await;
    if !restarted.is_ok_and(|status| status.success()) {
        println!("restart the daemon to use the new key: sudo launchctl kickstart -k system/dev.batonpass.BatonPass");
    }
    println!("Paired with {}; the key is in {}", from.ip(), key_path.display());
    Ok(())
}

/// `baton-pass pair CODE` on the desktop: proves the code shown on the Mac, then restarts the
/// service on the new key.
#[cfg(target_os = "linux")]
pub async fn on_desktop(code: &str, peer: Option<String>) -> Result<()> {
    let host = match peer {
        Some(peer) => peer,
        None => {
            let config = crate::config::Config::load()?;
            let first = config.peers.first().context("no peers configured; pass --peer <mac address>")?.clone();
            first.rsplit_once(':').filter(|(_, port)| port.parse::<u16>().is_ok()).map_or(first.clone(), |(host, _)| host.to_string())
        }
    };
    let key_path = crate::config::Config::key_path();
    join(&format!("{host}:{PORT}"), code, &key_path).await?;
    let _ = tokio::process::Command::new("systemctl").args(["--user", "try-restart", "baton-pass.service"]).status().await;
    println!("Paired with the Mac at {host}; the key is in {}", key_path.display());
    Ok(())
}

/// The key both sides derived, separated from any other use of the SPAKE2 output.
fn session_cipher(shared: Vec<u8>) -> Cipher {
    let mut hasher = Sha256::new();
    hasher.update(b"baton-pass pairing key");
    hasher.update(&shared);
    Cipher::from_secret(hasher.finalize().into())
}

async fn write_frame(stream: &mut TcpStream, frame: &[u8]) -> Result<()> {
    tokio::time::timeout(STEP_TIMEOUT, async {
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await?;
        stream.write_all(frame).await
    })
    .await
    .context("pairing timed out")??;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    tokio::time::timeout(STEP_TIMEOUT, async {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_FRAME {
            bail!("oversized pairing message");
        }
        let mut frame = vec![0u8; length];
        stream.read_exact(&mut frame).await?;
        Ok(frame)
    })
    .await
    .context("pairing timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("baton-pass-pair-{}-{name}", std::process::id())).join("key")
    }

    async fn pair(mac_code: &str, desktop_code: &str, mac_key: &Path, desktop_key: &Path) -> (Result<()>, Result<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let mac = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            host(&mut stream, mac_code, mac_key).await
        };
        tokio::join!(mac, join(&address, desktop_code, desktop_key))
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
    }

    #[test]
    fn the_same_code_puts_the_same_key_on_both_sides() {
        let (mac_key, desktop_key) = (temporary("mac-ok"), temporary("desktop-ok"));
        let (mac, desktop) = run(pair("1234-5678", "1234 5678", &mac_key, &desktop_key));
        mac.unwrap();
        desktop.unwrap();
        let (a, b) = (std::fs::read_to_string(&mac_key).unwrap(), std::fs::read_to_string(&desktop_key).unwrap());
        assert_eq!(a, b);
        assert_eq!(a.trim().len(), 64);
        Cipher::load(&mac_key).unwrap();
        for path in [mac_key, desktop_key] {
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_wrong_code_writes_no_key_anywhere() {
        let (mac_key, desktop_key) = (temporary("mac-bad"), temporary("desktop-bad"));
        let (mac, desktop) = run(pair("1234-5678", "1234-5679", &mac_key, &desktop_key));
        assert!(mac.is_err());
        assert!(desktop.is_err());
        assert!(!mac_key.exists() && !desktop_key.exists());
    }

    #[test]
    fn codes_are_eight_digits() {
        assert_eq!(normalize(&new_code()).unwrap().len(), 8);
        assert!(normalize("1234-567").is_err());
        assert!(normalize("1234-567a").is_err());
    }
}
