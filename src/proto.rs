use crate::clip::Clip;
pub const MAX_KEYS: usize = 32;

/// IPTOS_LOWDELAY. On Linux it gives the socket interactive priority, which the default fq
/// queueing discipline serves first: without it, a saturating upload (a torrent client) fills
/// the NIC queue and the kernel silently drops our datagrams.
pub const LOW_DELAY: u32 = 0x10;
pub const MAX_CLIPBOARD: usize = 32 * 1024 * 1024;

pub const FLAG_KEYBOARD_READY: u8 = 2;
pub const FLAG_POINTING_READY: u8 = 4;
pub const FLAG_VERSION_MISMATCH: u8 = 8;
pub const FLAG_ENTERED: u8 = 16;

/// Full keyboard state in HID terms: modifier bits, keyboard-page usages and consumer-page usages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyState {
    pub modifiers: u8,
    pub keys: Vec<u16>,
    pub consumer: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Hello { nonce: u64 },
    Challenge { nonce: u64, challenge: u64 },
    Join { nonce: u64, challenge: u64 },
    Welcome { nonce: u64, token: u64 },
    Reset { token: u64, counter: u64 },
    Heartbeat { entered: bool, keys: KeyState, buttons: u32 },
    Ack { flags: u8 },
    Enter { side: u8, frac: f32, speed: f32, exit: bool },
    Leave,
    Pointer { dx: f32, dy: f32, wheel_v: i32, wheel_h: i32, buttons: u32 },
    Keys(KeyState),
    Exit { frac: f32 },
    ClipPut { clip: Clip },
    ClipGet { have: [u8; 32] },
    ClipData { echo: u64, clip: Clip },
    ClipNone { echo: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub token: u64,
    pub counter: u64,
    pub msg: Msg,
}

impl Envelope {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.token.to_le_bytes());
        out.extend_from_slice(&self.counter.to_le_bytes());
        match &self.msg {
            Msg::Hello { nonce } => {
                out.push(0);
                out.extend_from_slice(&nonce.to_le_bytes());
            }
            Msg::Welcome { nonce, token } => {
                out.push(1);
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&token.to_le_bytes());
            }
            Msg::Reset { token, counter } => {
                out.push(2);
                out.extend_from_slice(&token.to_le_bytes());
                out.extend_from_slice(&counter.to_le_bytes());
            }
            Msg::Heartbeat { entered, keys, buttons } => {
                out.push(3);
                out.push(u8::from(*entered));
                put_keys(&mut out, keys);
                out.extend_from_slice(&buttons.to_le_bytes());
            }
            Msg::Ack { flags } => {
                out.push(4);
                out.push(*flags);
            }
            Msg::Enter { side, frac, speed, exit } => {
                out.push(5);
                out.push(*side);
                out.extend_from_slice(&frac.to_le_bytes());
                out.extend_from_slice(&speed.to_le_bytes());
                out.push(u8::from(*exit));
            }
            Msg::Leave => out.push(6),
            Msg::Pointer { dx, dy, wheel_v, wheel_h, buttons } => {
                out.push(7);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out.extend_from_slice(&wheel_v.to_le_bytes());
                out.extend_from_slice(&wheel_h.to_le_bytes());
                out.extend_from_slice(&buttons.to_le_bytes());
            }
            Msg::Keys(keys) => {
                out.push(8);
                put_keys(&mut out, keys);
            }
            Msg::Exit { frac } => {
                out.push(9);
                out.extend_from_slice(&frac.to_le_bytes());
            }
            Msg::ClipPut { clip } => {
                out.push(16);
                put_blob(&mut out, &crate::clip::encode(clip));
            }
            Msg::ClipGet { have } => {
                out.push(11);
                out.extend_from_slice(have);
            }
            Msg::ClipData { echo, clip } => {
                out.push(17);
                out.extend_from_slice(&echo.to_le_bytes());
                put_blob(&mut out, &crate::clip::encode(clip));
            }
            Msg::ClipNone { echo } => {
                out.push(13);
                out.extend_from_slice(&echo.to_le_bytes());
            }
            Msg::Challenge { nonce, challenge } => {
                out.push(14);
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&challenge.to_le_bytes());
            }
            Msg::Join { nonce, challenge } => {
                out.push(15);
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&challenge.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader(bytes);
        let token = reader.u64()?;
        let counter = reader.u64()?;
        let msg = match reader.u8()? {
            0 => Msg::Hello { nonce: reader.u64()? },
            1 => Msg::Welcome { nonce: reader.u64()?, token: reader.u64()? },
            2 => Msg::Reset { token: reader.u64()?, counter: reader.u64()? },
            3 => Msg::Heartbeat {
                entered: reader.u8()? != 0,
                keys: reader.keys()?,
                buttons: reader.u32()?,
            },
            4 => Msg::Ack { flags: reader.u8()? },
            5 => Msg::Enter { side: reader.u8()?, frac: reader.f32()?, speed: reader.f32()?, exit: reader.u8()? != 0 },
            6 => Msg::Leave,
            7 => Msg::Pointer {
                dx: reader.f32()?,
                dy: reader.f32()?,
                wheel_v: reader.i32()?,
                wheel_h: reader.i32()?,
                buttons: reader.u32()?,
            },
            8 => Msg::Keys(reader.keys()?),
            9 => Msg::Exit { frac: reader.f32()? },
            16 => Msg::ClipPut { clip: crate::clip::decode(reader.blob()?)? },
            11 => Msg::ClipGet { have: reader.take(32)?.try_into().ok()? },
            17 => Msg::ClipData { echo: reader.u64()?, clip: crate::clip::decode(reader.blob()?)? },
            13 => Msg::ClipNone { echo: reader.u64()? },
            14 => Msg::Challenge { nonce: reader.u64()?, challenge: reader.u64()? },
            15 => Msg::Join { nonce: reader.u64()?, challenge: reader.u64()? },
            _ => return None,
        };
        reader.0.is_empty().then_some(Self { token, counter, msg })
    }
}

fn put_keys(out: &mut Vec<u8>, keys: &KeyState) {
    out.push(keys.modifiers);
    for list in [&keys.keys, &keys.consumer] {
        let count = list.len().min(MAX_KEYS);
        out.push(count as u8);
        for usage in &list[..count] {
            out.extend_from_slice(&usage.to_le_bytes());
        }
    }
}

pub(crate) fn put_blob(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

/// The largest clipboard frame on the TCP exchange: a whole clipboard plus envelope overhead.
pub const MAX_CLIPBOARD_FRAME: usize = MAX_CLIPBOARD + 1024;

/// Writes one frame to a stream: its length as four little-endian bytes, then the bytes.
pub async fn write_frame<S: tokio::io::AsyncWrite + Unpin>(stream: &mut S, frame: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stream.write_all(&(frame.len() as u32).to_le_bytes()).await?;
    stream.write_all(frame).await
}

/// Reads one frame written by `write_frame`, refusing any longer than `max` bytes.
pub async fn read_frame<S: tokio::io::AsyncRead + Unpin>(stream: &mut S, max: usize) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length > max {
        anyhow::bail!("oversized frame ({length} bytes)");
    }
    let mut frame = vec![0u8; length];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

/// Reads little-endian fields and length-prefixed blobs off the front of a buffer.
pub(crate) struct Reader<'a>(pub(crate) &'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.0.len() < count {
            return None;
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Some(head)
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn usages(&mut self) -> Option<Vec<u16>> {
        let count = self.u8()? as usize;
        if count > MAX_KEYS {
            return None;
        }
        (0..count).map(|_| self.u16()).collect()
    }

    fn keys(&mut self) -> Option<KeyState> {
        Some(KeyState { modifiers: self.u8()?, keys: self.usages()?, consumer: self.usages()? })
    }

    pub(crate) fn blob(&mut self) -> Option<&'a [u8]> {
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

    fn round_trip(msg: Msg) {
        let envelope = Envelope { token: 0xdead_beef, counter: 42, msg };
        assert_eq!(Envelope::decode(&envelope.encode()), Some(envelope));
    }

    #[test]
    fn every_message_round_trips() {
        let keys = KeyState { modifiers: 0x09, keys: vec![4, 5, 0x29], consumer: vec![0xe9] };
        round_trip(Msg::Hello { nonce: 7 });
        round_trip(Msg::Welcome { nonce: 7, token: 9 });
        round_trip(Msg::Challenge { nonce: 7, challenge: 11 });
        round_trip(Msg::Join { nonce: 7, challenge: 11 });
        round_trip(Msg::Reset { token: 9, counter: 12 });
        round_trip(Msg::Heartbeat { entered: true, keys: keys.clone(), buttons: 5 });
        round_trip(Msg::Ack { flags: FLAG_ENTERED | FLAG_POINTING_READY });
        round_trip(Msg::Enter { side: 0, frac: 0.25, speed: 1.5, exit: true });
        round_trip(Msg::Leave);
        round_trip(Msg::Pointer { dx: -3.5, dy: 2.25, wheel_v: -120, wheel_h: 60, buttons: 1 });
        round_trip(Msg::Keys(keys));
        round_trip(Msg::Exit { frac: 0.5 });
        round_trip(Msg::ClipPut { clip: vec![("text/html".into(), "<i>hej</i>".into()), ("text/plain;charset=utf-8".into(), "hej".into())] });
        round_trip(Msg::ClipGet { have: [5; 32] });
        round_trip(Msg::ClipData { echo: 3, clip: vec![("image/png".into(), vec![1, 2, 3])] });
        round_trip(Msg::ClipNone { echo: 3 });
    }

    #[test]
    fn rejects_truncated_and_trailing_bytes() {
        let encoded = Envelope { token: 1, counter: 2, msg: Msg::Exit { frac: 0.5 } }.encode();
        assert!(Envelope::decode(&encoded[..encoded.len() - 1]).is_none());
        let mut longer = encoded.clone();
        longer.push(0);
        assert!(Envelope::decode(&longer).is_none());
    }

    #[test]
    fn rejects_oversized_key_lists() {
        let mut encoded = Envelope { token: 1, counter: 2, msg: Msg::Keys(KeyState::default()) }.encode();
        let count_index = encoded.len() - 2;
        encoded[count_index] = (MAX_KEYS + 1) as u8;
        assert!(Envelope::decode(&encoded).is_none());
    }
}
