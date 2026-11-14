use crate::proto::MAX_CLIPBOARD;
use anyhow::{Context, Result, bail};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const TEXT: &str = "text/plain;charset=utf-8";
const PNG: &str = "image/png";

pub type Content = (String, Vec<u8>);

/// Reads the general pasteboard as the console user, preferring whichever of image or text the
/// copying app listed first. Runs inside the user's session via `ferry clip-get`.
pub fn helper_get() -> Result<()> {
    let content = unsafe { read_pasteboard() };
    let mut stdout = std::io::stdout().lock();
    if let Some((mime, data)) = content {
        stdout.write_all(&(mime.len() as u32).to_le_bytes())?;
        stdout.write_all(mime.as_bytes())?;
        stdout.write_all(&data)?;
    }
    stdout.flush()?;
    Ok(())
}

unsafe fn read_pasteboard() -> Option<Content> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let types = pasteboard.types()?;
    let (png, tiff, text) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeString) };
    for offered in types.iter() {
        if &*offered == png {
            return pasteboard.dataForType(png).map(|data| (PNG.to_string(), data.to_vec()));
        }
        if &*offered == tiff {
            let data = pasteboard.dataForType(tiff)?;
            let rep = NSBitmapImageRep::imageRepWithData(&data)?;
            let png_data =
                unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
            return Some((PNG.to_string(), png_data.to_vec()));
        }
        if &*offered == text {
            return pasteboard.stringForType(text).map(|string| (TEXT.to_string(), string.to_string().into_bytes()));
        }
    }
    None
}

/// Replaces the general pasteboard with stdin as `mime`. Runs inside the user's session.
pub fn helper_set(mime: &str) -> Result<()> {
    let mut data = Vec::new();
    std::io::stdin().take(MAX_CLIPBOARD as u64 + 1).read_to_end(&mut data)?;
    if data.len() > MAX_CLIPBOARD {
        bail!("clipboard too large");
    }
    let pasteboard = NSPasteboard::generalPasteboard();
    unsafe {
        pasteboard.clearContents();
        match mime {
            PNG => {
                let png = NSData::with_bytes(&data);
                pasteboard.setData_forType(Some(&png), NSPasteboardTypePNG);
                if let Some(tiff) = NSBitmapImageRep::imageRepWithData(&png).and_then(|rep| rep.TIFFRepresentation()) {
                    pasteboard.setData_forType(Some(&tiff), NSPasteboardTypeTIFF);
                }
            }
            _ => {
                let text = String::from_utf8(data).context("clipboard text is not UTF-8")?;
                pasteboard.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
            }
        }
    }
    Ok(())
}

/// The user sitting at the Mac, if anyone is logged in at the console.
fn console_user() -> Option<(u32, String)> {
    let uid = std::fs::metadata("/dev/console").ok()?.uid();
    if uid == 0 {
        return None;
    }
    let passwd = unsafe { libc_getpwuid(uid) };
    if passwd.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*passwd).pw_name) }.to_str().ok()?.to_string();
    Some((uid, name))
}

#[repr(C)]
struct Passwd {
    pw_name: *const std::ffi::c_char,
}

unsafe extern "C" {
    #[link_name = "getpwuid"]
    fn libc_getpwuid(uid: u32) -> *const Passwd;
}

fn helper(user: &(u32, String), args: &[&str]) -> Result<Command> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new("/bin/launchctl");
    command
        .arg("asuser")
        .arg(user.0.to_string())
        .arg("/usr/bin/sudo")
        .arg("-n")
        .arg("-u")
        .arg(&user.1)
        .arg(executable)
        .args(args)
        .kill_on_drop(true);
    Ok(command)
}

pub async fn read() -> Result<Option<Content>> {
    let Some(user) = console_user() else { return Ok(None) };
    let output = tokio::time::timeout(Duration::from_secs(5), helper(&user, &["clip-get"])?.output())
        .await
        .context("clip-get timed out")??;
    if !output.status.success() {
        bail!("clip-get failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let bytes = output.stdout;
    if bytes.len() < 4 {
        return Ok(None);
    }
    let mime_length = u32::from_le_bytes(bytes[..4].try_into()?) as usize;
    if bytes.len() < 4 + mime_length {
        bail!("clip-get returned a truncated reply");
    }
    let mime = String::from_utf8(bytes[4..4 + mime_length].to_vec())?;
    let data = bytes[4 + mime_length..].to_vec();
    if data.is_empty() || data.len() > MAX_CLIPBOARD {
        return Ok(None);
    }
    Ok(Some((mime, data)))
}

pub async fn write(content: &Content) -> Result<()> {
    let Some(user) = console_user() else { return Ok(()) };
    let mime = if content.0 == PNG { PNG } else { TEXT };
    let mut child = helper(&user, &["clip-set", "--mime", mime])?
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(&content.1).await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .context("clip-set timed out")??;
    if !output.status.success() {
        bail!("clip-set failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}
