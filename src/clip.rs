//! The clipboard as ferry carries it between the machines: every format the copying app
//! offered that the other side understands, plus copied files as one bundle.

use crate::proto::MAX_CLIPBOARD;
use sha2::{Digest, Sha256};

pub const TEXT: &str = "text/plain;charset=utf-8";
pub const HTML: &str = "text/html";
pub const RTF: &str = "text/rtf";
pub const PNG: &str = "image/png";
pub const FILES: &str = "application/x-ferry-files";
const MAX_FLAVORS: usize = 8;
#[cfg(any(target_os = "macos", test))]
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

#[cfg(any(target_os = "macos", test))]
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

#[cfg(any(target_os = "linux", test))]
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

#[cfg(any(target_os = "macos", test))]
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

#[cfg(any(target_os = "macos", test))]
/// A relative path made only of ordinary names: no root, no `.` or `..`, no empty parts.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains('\0'))
}

fn put(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.0.len() < count {
            return None;
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn blob(&mut self) -> Option<&'a [u8]> {
        let length = self.u32()? as usize;
        if length > MAX_CLIPBOARD {
            return None;
        }
        self.take(length)
    }
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
    fn bundles_cannot_escape_their_folder() {
        for path in ["../x", "/etc/passwd", "a/../../x", "a//b", "./a", "", "a/."] {
            assert!(!safe_path(path), "{path}");
            let file = File { path: path.into(), executable: false, data: Vec::new() };
            assert_eq!(decode_files(&encode_files(&[file])), None, "{path}");
        }
        assert!(safe_path("Folder/sub dir/file name.txt"));
    }
}
