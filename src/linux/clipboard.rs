use crate::clip::{self, Clip, FILES, HTML, PNG, RTF, TEXT};
use crate::crypto::Cipher;
use crate::proto::{Envelope, MAX_CLIPBOARD};
use anyhow::{Context, Result, anyhow, bail};
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use wl_clipboard_rs::{copy, paste};

const TIMEOUT: Duration = Duration::from_secs(5);
const TEXT_TYPES: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING", "TEXT"];
const RTF_TYPES: [&str; 2] = ["text/rtf", "application/rtf"];

/// The desktop clipboard, read and written over Wayland's data-control protocol.
pub struct Clipboard {
    serving: Arc<AtomicBool>,
}

impl Clipboard {
    pub fn new() -> Self {
        Self { serving: Arc::new(AtomicBool::new(false)) }
    }

    /// True while the clipboard still holds what Baton Pass last put there, so there is nothing new
    /// to carry over.
    pub fn holds_ours(&self) -> bool {
        self.serving.load(Ordering::Acquire)
    }

    /// Reads every format Baton Pass carries from the clipboard; files come along as a bundle.
    pub async fn read(&self) -> Result<Option<Clip>> {
        tokio::time::timeout(TIMEOUT, tokio::task::spawn_blocking(read_blocking))
            .await
            .context("reading the clipboard timed out")?
            .context("clipboard reader")?
    }

    /// Offers `clip` on the clipboard in every format it carries, served from a thread until
    /// something else is copied.
    pub async fn write(&self, clip: &Clip) -> Result<()> {
        let sources = match clip::get(clip, FILES) {
            Some(bundle) => file_sources(bundle)?,
            None => sources(clip),
        };
        if sources.is_empty() {
            return Ok(());
        }
        let (ready, prepared) = tokio::sync::oneshot::channel();
        let serving = self.serving.clone();
        std::thread::Builder::new().name("baton-pass-clipboard".into()).spawn(move || {
            let mut options = copy::Options::new();
            options.foreground(true);
            match options.prepare_copy_multi(sources) {
                Ok(copy) => {
                    serving.store(true, Ordering::Release);
                    let _ = ready.send(Ok(()));
                    if let Err(error) = copy.serve() {
                        tracing::debug!("serving the clipboard: {error}");
                    }
                    serving.store(false, Ordering::Release);
                }
                Err(error) => {
                    let _ = ready.send(Err(anyhow!(error)));
                }
            }
        })?;
        tokio::time::timeout(TIMEOUT, prepared).await.context("setting the clipboard timed out")?.context("clipboard writer")?
    }
}

fn sources(clip: &Clip) -> Vec<copy::MimeSource> {
    let mut sources = Vec::new();
    for (mime, data) in clip {
        let source = || copy::Source::Bytes(data.clone().into_boxed_slice());
        match mime.as_str() {
            TEXT => sources.push(copy::MimeSource { source: source(), mime_type: copy::MimeType::Text }),
            HTML | PNG => sources.push(copy::MimeSource { source: source(), mime_type: copy::MimeType::Specific(mime.clone()) }),
            RTF => {
                for rtf in RTF_TYPES {
                    sources.push(copy::MimeSource { source: source(), mime_type: copy::MimeType::Specific(rtf.into()) });
                }
            }
            _ => {}
        }
    }
    sources
}

fn read_blocking() -> Result<Option<Clip>> {
    let offered = match paste::get_mime_types_ordered(paste::ClipboardType::Regular, paste::Seat::Unspecified) {
        Ok(offered) => offered,
        Err(paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => return Ok(None),
        Err(error) => return Err(error).context("listing the clipboard's formats"),
    };
    let has = |mime: &str| offered.iter().any(|offered| offered == mime);
    let text_type = offered.iter().find(|offered| TEXT_TYPES.contains(&offered.as_str())).cloned();
    let text = match &text_type {
        Some(offered) => fetch(offered)?.map(|data| utf8_text(offered, data)),
        None => None,
    };
    let mut clip: Clip = Vec::new();
    if has("text/uri-list")
        && let Some(list) = fetch("text/uri-list")?
        && let Some(files) = files_from_uri_list(&String::from_utf8_lossy(&list))
    {
        clip.push((FILES.into(), files));
    } else if has(PNG) {
        if let Some(png) = fetch(PNG)? {
            clip.push((PNG.into(), png));
        }
    } else {
        if has(HTML)
            && let Some(html) = fetch(HTML)?
        {
            clip.push((HTML.into(), html));
        }
        if let Some(rtf_type) = RTF_TYPES.iter().find(|rtf| has(rtf))
            && let Some(rtf) = fetch(rtf_type)?
        {
            clip.push((RTF.into(), rtf));
        }
    }
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        clip.push((TEXT.into(), text));
    }
    if clip.is_empty() || clip::size(&clip) > MAX_CLIPBOARD {
        return Ok(None);
    }
    Ok(Some(clip))
}

fn fetch(mime: &str) -> Result<Option<Vec<u8>>> {
    let (pipe, _) = match paste::get_contents(paste::ClipboardType::Regular, paste::Seat::Unspecified, paste::MimeType::Specific(mime)) {
        Ok(contents) => contents,
        Err(paste::Error::NoSeats | paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {mime} from the clipboard")),
    };
    let mut data = Vec::new();
    pipe.take(MAX_CLIPBOARD as u64 + 1).read_to_end(&mut data)?;
    Ok((!data.is_empty() && data.len() <= MAX_CLIPBOARD).then_some(data))
}

/// X11's STRING target is Latin-1; everything sent on is UTF-8.
fn utf8_text(offered: &str, data: Vec<u8>) -> Vec<u8> {
    if offered == "STRING" {
        return data.iter().map(|byte| char::from(*byte)).collect::<String>().into_bytes();
    }
    match String::from_utf8(data) {
        Ok(text) => text.into_bytes(),
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned().into_bytes(),
    }
}

/// Bundles the local files a file manager copied, folders included. Gives up (and the copy
/// travels as text only) when an entry is not a local file or the files are too large.
fn files_from_uri_list(list: &str) -> Option<Vec<u8>> {
    let paths: Vec<PathBuf> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(clip::local_path)
        .collect::<Option<_>>()?;
    if paths.is_empty() {
        return None;
    }
    clip::bundle_paths(&paths)
}

/// Unpacks files from the Mac under ~/.cache/baton-pass/clipboard and offers them the way file
/// managers copy files, so pasting in Dolphin copies them.
fn file_sources(bundle: &[u8]) -> Result<Vec<copy::MimeSource>> {
    let files = clip::decode_files(bundle).context("malformed file bundle")?;
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .context("no cache folder")?;
    let items = clip::unpack(&files, &cache.join("baton-pass/clipboard"))?;
    let uris: Vec<String> = items.iter().map(|item| clip::file_uri(item)).collect();
    let paths: Vec<String> = items.iter().map(|item| item.display().to_string()).collect();
    let bytes = |text: String| copy::Source::Bytes(text.into_bytes().into_boxed_slice());
    Ok(vec![
        copy::MimeSource { source: bytes(uris.iter().map(|uri| format!("{uri}\r\n")).collect()), mime_type: copy::MimeType::Specific("text/uri-list".into()) },
        copy::MimeSource { source: bytes(format!("copy\n{}", uris.join("\n"))), mime_type: copy::MimeType::Specific("x-special/gnome-copied-files".into()) },
        copy::MimeSource { source: bytes(paths.join("\n")), mime_type: copy::MimeType::Text },
    ])
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_strings_become_utf8() {
        assert_eq!(utf8_text("STRING", vec![0x68, 0xe9]), "hé".as_bytes());
        assert_eq!(utf8_text("UTF8_STRING", "hé".as_bytes().to_vec()), "hé".as_bytes());
        assert_eq!(utf8_text("TEXT", vec![0x68, 0xff]), "h\u{fffd}".as_bytes());
    }

    #[test]
    fn non_file_uris_travel_as_text() {
        assert_eq!(files_from_uri_list("https://example.com/x\n"), None);
    }
}
