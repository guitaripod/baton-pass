use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const HEADER: [u8; 3] = *b"FY\x01";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
pub const OVERHEAD: usize = HEADER.len() + NONCE_LEN + TAG_LEN;

pub struct Cipher(XChaCha20Poly1305);

impl Cipher {
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading key {} (run `ferry keygen`)", path.display()))?;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("{} must not be readable by group or others (chmod 600)", path.display());
        }
        let text = std::fs::read_to_string(path)?;
        let key = decode_hex(text.trim()).context("key file must hold 64 hex characters")?;
        Ok(Self(XChaCha20Poly1305::new(key.as_slice().into())))
    }

    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("system randomness");
        let mut packet = Vec::with_capacity(plaintext.len() + OVERHEAD);
        packet.extend_from_slice(&HEADER);
        packet.extend_from_slice(&nonce);
        let mut body = plaintext.to_vec();
        self.0
            .encrypt_in_place(XNonce::from_slice(&nonce), &HEADER, &mut body)
            .expect("encryption of a bounded buffer");
        packet.extend_from_slice(&body);
        packet
    }

    pub fn open(&self, packet: &[u8]) -> Option<Vec<u8>> {
        if packet.len() < OVERHEAD || packet[..HEADER.len()] != HEADER {
            return None;
        }
        let nonce = XNonce::from_slice(&packet[HEADER.len()..HEADER.len() + NONCE_LEN]);
        let mut body = packet[HEADER.len() + NONCE_LEN..].to_vec();
        self.0.decrypt_in_place(nonce, &HEADER, &mut body).ok()?;
        Some(body)
    }
}

pub fn ensure_key(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|error| anyhow::anyhow!("system randomness: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writeln!(file, "{}", encode_hex(&key))?;
    Ok(())
}

pub fn random_u64() -> u64 {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("system randomness");
    u64::from_le_bytes(bytes)
}

/// Monotonic per-process counter seeded from wall-clock microseconds, so a restarted sender
/// keeps counting above everything its previous incarnation sent.
pub struct Counter(u64);

impl Counter {
    pub fn new() -> Self {
        let micros = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_micros() as u64)
            .unwrap_or(1);
        Self(micros)
    }

    pub fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher(XChaCha20Poly1305::new(&[7u8; 32].into()))
    }

    #[test]
    fn round_trip() {
        let cipher = cipher();
        let packet = cipher.seal(b"hello");
        assert_eq!(cipher.open(&packet).as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn rejects_tampering() {
        let cipher = cipher();
        let mut packet = cipher.seal(b"hello");
        let last = packet.len() - 1;
        packet[last] ^= 1;
        assert!(cipher.open(&packet).is_none());
        let mut header = cipher.seal(b"hello");
        header[2] = 2;
        assert!(cipher.open(&header).is_none());
    }

    #[test]
    fn rejects_other_key() {
        let packet = cipher().seal(b"hello");
        let other = Cipher(XChaCha20Poly1305::new(&[8u8; 32].into()));
        assert!(other.open(&packet).is_none());
    }

    #[test]
    fn hex_round_trip() {
        let key = [0xabu8; 32];
        assert_eq!(decode_hex(&encode_hex(&key)), Some(key));
        assert_eq!(decode_hex("zz"), None);
    }
}
