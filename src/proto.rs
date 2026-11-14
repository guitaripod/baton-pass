pub const MAX_KEYS: usize = 32;
pub const MAX_CLIPBOARD: usize = 32 * 1024 * 1024;

#[cfg(target_os = "macos")]
pub const FLAG_VHID_CONNECTED: u8 = 1;
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
    Welcome { nonce: u64, token: u64 },
    Reset,
    Heartbeat { entered: bool, keys: KeyState, buttons: u32 },
    Ack { flags: u8 },
    Enter { side: u8, frac: f32, speed: f32 },
    Leave,
    Pointer { dx: f32, dy: f32, wheel_v: i32, wheel_h: i32, buttons: u32 },
    Keys(KeyState),
    Exit { frac: f32 },
    ClipPut { mime: String, data: Vec<u8> },
    ClipGet,
    ClipData { mime: String, data: Vec<u8> },
    ClipNone,
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
            Msg::Reset => out.push(2),
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
            Msg::Enter { side, frac, speed } => {
                out.push(5);
                out.push(*side);
                out.extend_from_slice(&frac.to_le_bytes());
                out.extend_from_slice(&speed.to_le_bytes());
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
            Msg::ClipPut { mime, data } => {
                out.push(10);
                put_blob(&mut out, mime.as_bytes());
                put_blob(&mut out, data);
            }
            Msg::ClipGet => out.push(11),
            Msg::ClipData { mime, data } => {
                out.push(12);
                put_blob(&mut out, mime.as_bytes());
                put_blob(&mut out, data);
            }
            Msg::ClipNone => out.push(13),
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
            2 => Msg::Reset,
            3 => Msg::Heartbeat {
                entered: reader.u8()? != 0,
                keys: reader.keys()?,
                buttons: reader.u32()?,
            },
            4 => Msg::Ack { flags: reader.u8()? },
            5 => Msg::Enter { side: reader.u8()?, frac: reader.f32()?, speed: reader.f32()? },
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
            10 => Msg::ClipPut { mime: reader.text()?, data: reader.blob()? },
            11 => Msg::ClipGet,
            12 => Msg::ClipData { mime: reader.text()?, data: reader.blob()? },
            13 => Msg::ClipNone,
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

fn put_blob(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, count: usize) -> Option<&[u8]> {
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

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
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

    fn blob(&mut self) -> Option<Vec<u8>> {
        let length = self.u32()? as usize;
        if length > MAX_CLIPBOARD {
            return None;
        }
        Some(self.take(length)?.to_vec())
    }

    fn text(&mut self) -> Option<String> {
        String::from_utf8(self.blob()?).ok()
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
        round_trip(Msg::Reset);
        round_trip(Msg::Heartbeat { entered: true, keys: keys.clone(), buttons: 5 });
        round_trip(Msg::Ack { flags: FLAG_ENTERED | FLAG_POINTING_READY });
        round_trip(Msg::Enter { side: 0, frac: 0.25, speed: 1.5 });
        round_trip(Msg::Leave);
        round_trip(Msg::Pointer { dx: -3.5, dy: 2.25, wheel_v: -120, wheel_h: 60, buttons: 1 });
        round_trip(Msg::Keys(keys));
        round_trip(Msg::Exit { frac: 0.5 });
        round_trip(Msg::ClipPut { mime: "text/plain;charset=utf-8".into(), data: "hej".into() });
        round_trip(Msg::ClipGet);
        round_trip(Msg::ClipData { mime: "image/png".into(), data: vec![1, 2, 3] });
        round_trip(Msg::ClipNone);
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
