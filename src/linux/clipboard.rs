use crate::crypto::Cipher;
use crate::proto::{Envelope, MAX_CLIPBOARD};
use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

const TEXT: &str = "text/plain;charset=utf-8";
const PNG: &str = "image/png";
const TIMEOUT: Duration = Duration::from_secs(5);

pub type Content = (String, Vec<u8>);

pub fn digest(content: &Content) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(content.0.as_bytes());
    hasher.update([0]);
    hasher.update(&content.1);
    hasher.finalize().into()
}

/// Reads the Wayland clipboard, preferring whichever of PNG or text the source offered first.
pub async fn read() -> Result<Option<Content>> {
    let listing = run_wl_paste(&["--list-types"]).await?;
    let Some(listing) = listing else { return Ok(None) };
    let listing = String::from_utf8_lossy(&listing);
    let chosen = listing.lines().map(str::trim).find_map(|offered| match offered {
        PNG => Some((offered.to_string(), PNG)),
        "text/plain;charset=utf-8" | "UTF8_STRING" | "text/plain" | "STRING" | "TEXT" => {
            Some((offered.to_string(), TEXT))
        }
        _ => None,
    });
    let Some((offered, mime)) = chosen else { return Ok(None) };
    let Some(data) = run_wl_paste(&["--no-newline", "--type", &offered]).await? else {
        return Ok(None);
    };
    if data.is_empty() || data.len() > MAX_CLIPBOARD {
        return Ok(None);
    }
    Ok(Some((mime.to_string(), data)))
}

async fn run_wl_paste(args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = tokio::time::timeout(
        TIMEOUT,
        Command::new("wl-paste").args(args).stdin(Stdio::null()).kill_on_drop(true).output(),
    )
    .await
    .context("wl-paste timed out")?
    .context("running wl-paste")?;
    Ok(output.status.success().then_some(output.stdout))
}

pub async fn write(content: &Content) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", &content.0])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running wl-copy")?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(&content.1).await?;
    drop(stdin);
    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output()).await.context("wl-copy timed out")??;
    if !output.status.success() {
        bail!("wl-copy failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// One request/response exchange with the Mac over TCP, authenticated like the datagrams.
pub async fn exchange(cipher: &Cipher, peer: SocketAddr, frame: Vec<u8>, token: u64) -> Result<Envelope> {
    tokio::time::timeout(TIMEOUT, async {
        let mut stream = TcpStream::connect(peer).await.context("connecting to the Mac")?;
        stream.set_nodelay(true)?;
        stream.write_all(&(frame.len() as u32).to_le_bytes()).await?;
        stream.write_all(&frame).await?;
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_CLIPBOARD + 1024 {
            bail!("oversized reply");
        }
        let mut reply = vec![0u8; length];
        stream.read_exact(&mut reply).await?;
        let envelope = cipher
            .open(&reply)
            .and_then(|plain| Envelope::decode(&plain))
            .ok_or_else(|| anyhow!("unauthenticated reply"))?;
        if envelope.token != token {
            bail!("reply from another session");
        }
        Ok(envelope)
    })
    .await
    .context("clipboard exchange timed out")?
}
