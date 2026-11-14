use crate::clip::{self, Clip, FILES, HTML, PNG, RTF, TEXT};
use crate::proto::MAX_CLIPBOARD;
use anyhow::{Context, Result, bail};
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypeHTML, NSPasteboardTypePNG,
    NSPasteboardTypeRTF, NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString, NSURL};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Reads the general pasteboard as the console user and writes its change count and every
/// format ferry carries to stdout. Runs inside the user's session via `ferry clip-get`.
pub fn helper_get() -> Result<()> {
    let pasteboard = NSPasteboard::generalPasteboard();
    let change = pasteboard.changeCount() as i64;
    let content = unsafe { read_pasteboard(&pasteboard) }.unwrap_or_default();
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&change.to_le_bytes())?;
    stdout.write_all(&clip::encode(&content))?;
    stdout.flush()?;
    Ok(())
}

/// The pasteboard as a clip: an image (PNG, or TIFF converted to PNG), or HTML and RTF, each
/// with the plain text alongside when the copying app offered it.
unsafe fn read_pasteboard(pasteboard: &NSPasteboard) -> Option<Clip> {
    let types = pasteboard.types()?;
    let (png, tiff, html, rtf, text) = unsafe {
        (NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeHTML, NSPasteboardTypeRTF, NSPasteboardTypeString)
    };
    let has = |wanted: &NSString| types.iter().any(|offered| &*offered == wanted);
    let mut content: Clip = Vec::new();
    if has(png) {
        content.extend(pasteboard.dataForType(png).map(|data| (PNG.to_string(), data.to_vec())));
    } else if has(tiff) {
        let data = pasteboard.dataForType(tiff)?;
        let rep = NSBitmapImageRep::imageRepWithData(&data)?;
        let png_data =
            unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
        content.push((PNG.to_string(), png_data.to_vec()));
    } else {
        if has(html) {
            content.extend(pasteboard.dataForType(html).map(|data| (HTML.to_string(), data.to_vec())));
        }
        if has(rtf) {
            content.extend(pasteboard.dataForType(rtf).map(|data| (RTF.to_string(), data.to_vec())));
        }
    }
    if has(text) {
        content.extend(pasteboard.stringForType(text).map(|string| (TEXT.to_string(), string.to_string().into_bytes())));
    }
    content.retain(|(_, data)| !data.is_empty());
    (!content.is_empty() && clip::size(&content) <= MAX_CLIPBOARD).then_some(content)
}

/// Replaces the general pasteboard with the clip on stdin and writes the pasteboard's new
/// change count to stdout. Files are unpacked into a folder in the user's caches first, and
/// the pasteboard gets their URLs, so pasting in Finder copies them. Runs inside the user's
/// session via `ferry clip-set`.
pub fn helper_set() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin().take(MAX_CLIPBOARD as u64 + 4096).read_to_end(&mut bytes)?;
    let content = clip::decode(&bytes).context("malformed clipboard")?;
    let pasteboard = NSPasteboard::generalPasteboard();
    if let Some(bundle) = clip::get(&content, FILES) {
        let files = clip::decode_files(bundle).context("malformed file bundle")?;
        let items = unpack(&files)?;
        let urls: Vec<_> = items
            .iter()
            .map(|path| ProtocolObject::<dyn NSPasteboardWriting>::from_retained(NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))))
            .collect();
        pasteboard.clearContents();
        if !pasteboard.writeObjects(&NSArray::from_retained_slice(&urls)) {
            bail!("the pasteboard refused the copied files");
        }
    } else {
        pasteboard.clearContents();
        for (mime, data) in &content {
            unsafe { put(&pasteboard, mime, data)? };
        }
    }
    std::io::stdout().write_all(&(pasteboard.changeCount() as i64).to_le_bytes())?;
    Ok(())
}

unsafe fn put(pasteboard: &NSPasteboard, mime: &str, data: &[u8]) -> Result<()> {
    match mime {
        TEXT => {
            let text = std::str::from_utf8(data).context("clipboard text is not UTF-8")?;
            unsafe { pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) };
        }
        HTML => unsafe {
            pasteboard.setData_forType(Some(&NSData::with_bytes(data)), NSPasteboardTypeHTML);
        },
        RTF => unsafe {
            pasteboard.setData_forType(Some(&NSData::with_bytes(data)), NSPasteboardTypeRTF);
        },
        PNG => {
            let png = NSData::with_bytes(data);
            let rep = NSBitmapImageRep::imageRepWithData(&png).context("clipboard image is not a PNG")?;
            unsafe { pasteboard.setData_forType(Some(&png), NSPasteboardTypePNG) };
            if let Some(tiff) = rep.TIFFRepresentation() {
                unsafe { pasteboard.setData_forType(Some(&tiff), NSPasteboardTypeTIFF) };
            }
        }
        _ => {}
    }
    Ok(())
}

/// Writes the bundled files into a fresh folder under ~/Library/Caches/dev.ferry.Ferry (the
/// previous one is removed) and returns the top-level items, as the desktop had them selected.
fn unpack(files: &[clip::File]) -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let parent = Path::new(&home).join("Library/Caches/dev.ferry.Ferry/Clipboard");
    if parent.exists() {
        std::fs::remove_dir_all(&parent).with_context(|| format!("clearing {}", parent.display()))?;
    }
    let mut items: Vec<PathBuf> = Vec::new();
    for file in files {
        let target = parent.join(&file.path);
        if let Some(folder) = target.parent() {
            std::fs::create_dir_all(folder)?;
        }
        std::fs::write(&target, &file.data).with_context(|| format!("writing {}", target.display()))?;
        if file.executable {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
        }
        let top = parent.join(file.path.split('/').next().unwrap_or(&file.path));
        if !items.contains(&top) {
            items.push(top);
        }
    }
    Ok(items)
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

/// The Mac's clipboard and its change count, read in the console user's session.
pub async fn read() -> Result<Option<(i64, Clip)>> {
    let Some(user) = console_user() else { return Ok(None) };
    let output = tokio::time::timeout(Duration::from_secs(5), helper(&user, &["clip-get"])?.output())
        .await
        .context("clip-get timed out")??;
    if !output.status.success() {
        bail!("clip-get failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let bytes = output.stdout;
    if bytes.len() < 8 {
        bail!("clip-get returned a truncated reply");
    }
    let change = i64::from_le_bytes(bytes[..8].try_into()?);
    let content = clip::decode(&bytes[8..]).context("clip-get returned a malformed clipboard")?;
    Ok(Some((change, content)))
}

/// Puts `content` on the Mac's clipboard and returns the pasteboard's change count after.
pub async fn write(content: &Clip) -> Result<i64> {
    let Some(user) = console_user() else { bail!("nobody is logged in at the Mac") };
    let mut child = helper(&user, &["clip-set"])?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(&clip::encode(content)).await?;
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .context("clip-set timed out")??;
    if !output.status.success() {
        bail!("clip-set failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let change: [u8; 8] = output.stdout.get(..8).context("clip-set did not report the change count")?.try_into()?;
    Ok(i64::from_le_bytes(change))
}
