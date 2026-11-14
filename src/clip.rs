//! The clipboard as Baton Pass carries it between the machines: every format the copying app
//! offered that the other side understands, plus copied files as one bundle.

use crate::proto::{MAX_CLIPBOARD, Reader, put_blob as put};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const TEXT: &str = "text/plain;charset=utf-8";
pub const HTML: &str = "text/html";
pub const RTF: &str = "text/rtf";
pub const PNG: &str = "image/png";
pub const FILES: &str = "application/x-baton-pass-files";
const MAX_FLAVORS: usize = 8;
const MAX_FILES: usize = 10_000;

/// One format of the clipboard: its MIME type and bytes.
pub type Flavor = (String, Vec<u8>);

/// The whole clipboard, in the order the copying app preferred.
pub type Clip = Vec<Flavor>;

pub fn digest(clip: &Clip) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (mime, data) in clip {
        hasher.update((mime.len() as u32).to_le_bytes());
        hasher.update(mime.as_bytes());
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update(data);
    }
    hasher.finalize().into()
}

pub fn size(clip: &Clip) -> usize {
    clip.iter().map(|(mime, data)| mime.len() + data.len()).sum()
}

/// "text/html, text/plain (1234 bytes)", for logs.
pub fn describe(clip: &Clip) -> String {
    let mimes: Vec<&str> = clip.iter().map(|(mime, _)| mime.as_str()).collect();
    format!("{} ({} bytes)", mimes.join(", "), size(clip))
}

pub fn get<'a>(clip: &'a Clip, mime: &str) -> Option<&'a [u8]> {
    clip.iter().find(|(offered, _)| offered == mime).map(|(_, data)| data.as_slice())
}

pub fn encode(clip: &Clip) -> Vec<u8> {
    let mut out = Vec::with_capacity(size(clip) + 16);
    let count = clip.len().min(MAX_FLAVORS);
    out.push(count as u8);
    for (mime, data) in &clip[..count] {
        put(&mut out, mime.as_bytes());
        put(&mut out, data);
    }
    out
}

pub fn decode(bytes: &[u8]) -> Option<Clip> {
    let mut reader = Reader(bytes);
    let count = reader.u8()? as usize;
    if count > MAX_FLAVORS {
        return None;
    }
    let mut clip = Vec::with_capacity(count);
    for _ in 0..count {
        let mime = String::from_utf8(reader.blob()?.to_vec()).ok()?;
        clip.push((mime, reader.blob()?.to_vec()));
    }
    (reader.0.is_empty() && size(&clip) <= MAX_CLIPBOARD).then_some(clip)
}

/// A copied file, by its path relative to the folder the copied items sit in.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub path: String,
    pub executable: bool,
    pub data: Vec<u8>,
}

pub fn encode_files(files: &[File]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for file in files {
        put(&mut out, file.path.as_bytes());
        out.push(u8::from(file.executable));
        put(&mut out, &file.data);
    }
    out
}

/// Decodes a files bundle, refusing any path that could land outside the folder it is
/// written into.
pub fn decode_files(bytes: &[u8]) -> Option<Vec<File>> {
    let mut reader = Reader(bytes);
    let count = reader.u32()? as usize;
    if count > MAX_FILES {
        return None;
    }
    let mut files = Vec::with_capacity(count);
    for _ in 0..count {
        let path = String::from_utf8(reader.blob()?.to_vec()).ok()?;
        if !safe_path(&path) {
            return None;
        }
        let executable = reader.u8()? != 0;
        files.push(File { path, executable, data: reader.blob()?.to_vec() });
    }
    reader.0.is_empty().then_some(files)
}

/// A relative path made only of ordinary names: no root, no `.` or `..`, no empty parts.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
}

/// Bundles local files and folders (walked recursively) under their own names, as the file
/// manager had them selected. Symbolic links and special files are left out. None when nothing
/// could be read (a folder macOS keeps private, for one) or the total is over MAX_CLIPBOARD.
pub fn bundle_paths(paths: &[PathBuf]) -> Option<Vec<u8>> {
    let mut files = Vec::new();
    let mut total = 0usize;
    for path in paths {
        let name = path.file_name()?.to_str()?.to_string();
        collect(path, &name, &mut files, &mut total)?;
    }
    if files.is_empty() {
        return None;
    }
    let bundle = encode_files(&files);
    (bundle.len() <= MAX_CLIPBOARD).then_some(bundle)
}

fn collect(path: &Path, name: &str, files: &mut Vec<File>, total: &mut usize) -> Option<()> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.is_file() {
        *total += metadata.len() as usize;
        if *total > MAX_CLIPBOARD {
            tracing::info!("copied files are over {} MB; only their names travel", MAX_CLIPBOARD >> 20);
            return None;
        }
        let data = std::fs::read(path).ok()?;
        let executable = metadata.permissions().mode() & 0o111 != 0;
        files.push(File { path: name.to_string(), executable, data });
    } else if metadata.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path).ok()?.filter_map(|entry| entry.ok()).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let child = entry.file_name().into_string().ok()?;
            collect(&entry.path(), &format!("{name}/{child}"), files, total)?;
        }
    }
    Some(())
}

/// Writes bundled files into `parent`, emptied first, and returns the top-level items in the
/// order they were copied.
pub fn unpack(files: &[File], parent: &Path) -> Result<Vec<PathBuf>> {
    if parent.exists() {
        std::fs::remove_dir_all(parent).with_context(|| format!("clearing {}", parent.display()))?;
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

/// The local path of a `file://` URI, percent-decoded.
pub fn local_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
}

/// A `file://` URI for a local path, with everything but unreserved characters and `/`
/// percent-encoded.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => uri.push(*byte as char),
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_round_trip_and_reject_damage() {
        let clip: Clip = vec![(HTML.into(), b"<b>hi</b>".to_vec()), (TEXT.into(), b"hi".to_vec())];
        let bytes = encode(&clip);
        assert_eq!(decode(&bytes), Some(clip.clone()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_ne!(digest(&clip), digest(&vec![(TEXT.into(), b"hi".to_vec())]));
        assert_eq!(get(&clip, TEXT), Some(&b"hi"[..]));
    }

    #[test]
    fn file_bundles_round_trip() {
        let files = vec![
            File { path: "notes.txt".into(), executable: false, data: b"a".to_vec() },
            File { path: "tools/run.sh".into(), executable: true, data: b"#!/bin/sh".to_vec() },
        ];
        assert_eq!(decode_files(&encode_files(&files)), Some(files));
    }

    #[test]
    fn file_uris_round_trip() {
        let path = Path::new("/home/m/My File #1 ä.txt");
        assert_eq!(local_path(&file_uri(path)).as_deref(), Some(path));
        assert_eq!(local_path("file://localhost/tmp/a"), Some(PathBuf::from("/tmp/a")));
        assert_eq!(local_path("https://example.com/a"), None);
        assert_eq!(local_path("file://otherhost/a"), None);
        assert_eq!(local_path("file:///bad%zz"), None);
    }

    #[test]
    fn copied_folders_bundle_and_unpack() {
        let root = std::env::temp_dir().join(format!("baton-pass-bundle-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Folder/sub")).unwrap();
        std::fs::write(root.join("Folder/a.txt"), b"a").unwrap();
        std::fs::write(root.join("Folder/sub/b.sh"), b"bb").unwrap();
        std::fs::set_permissions(root.join("Folder/sub/b.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(root.join("single.txt"), b"s").unwrap();
        let bundle = bundle_paths(&[root.join("Folder"), root.join("single.txt")]).unwrap();
        let files = decode_files(&bundle).unwrap();
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, ["Folder/a.txt", "Folder/sub/b.sh", "single.txt"]);
        let out = root.join("out");
        let items = unpack(&files, &out).unwrap();
        assert_eq!(items, [out.join("Folder"), out.join("single.txt")]);
        assert_eq!(std::fs::read(out.join("Folder/sub/b.sh")).unwrap(), b"bb");
        assert_ne!(std::fs::metadata(out.join("Folder/sub/b.sh")).unwrap().permissions().mode() & 0o111, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn bundles_cannot_escape_their_folder() {
        for path in ["../x", "/etc/passwd", "a/../../x", "a//b", "./a", "", "a/."] {
            assert!(!safe_path(path), "{path}");
            let file = File { path: path.into(), executable: false, data: Vec::new() };
            assert_eq!(decode_files(&encode_files(&[file])), None, "{path}");
        }
        assert!(safe_path("Folder/sub dir/file name.txt"));
    }
}
