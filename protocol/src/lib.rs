#![no_std]

pub const MAX_PAYLOAD: usize = 192;
pub const MAX_WIRE: usize = 2 * (MAX_PAYLOAD + 5) + 2;
const FLAG: u8 = 0x7e;
const ESC: u8 = 0x7d;
pub const AUDIO: u8 = b'U';
pub const PLAY: u8 = b'P';
pub const MODEM: u8 = b'M';
pub const ANSWER: u8 = b'A';
pub const HANGUP: u8 = b'H';
pub const DIAL: u8 = b'D';
pub const QUERY: u8 = b'Q';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub len: usize,
    pub data: [u8; MAX_PAYLOAD],
}
impl Frame {
    pub fn new(kind: u8, payload: &[u8]) -> Option<Self> {
        if payload.len() > MAX_PAYLOAD {
            return None;
        }
        let mut data = [0; MAX_PAYLOAD];
        data[..payload.len()].copy_from_slice(payload);
        Some(Self {
            kind,
            len: payload.len(),
            data,
        })
    }
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &b in bytes {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = (crc >> 1) ^ if crc & 1 != 0 { 0xa001 } else { 0 };
        }
    }
    crc
}
pub fn encode(frame: &Frame, out: &mut [u8; MAX_WIRE]) -> usize {
    let mut raw = [0u8; MAX_PAYLOAD + 5];
    raw[0] = frame.kind;
    raw[1..3].copy_from_slice(&(frame.len as u16).to_le_bytes());
    raw[3..3 + frame.len].copy_from_slice(frame.payload());
    let crc = crc16(&raw[..3 + frame.len]);
    raw[3 + frame.len..5 + frame.len].copy_from_slice(&crc.to_le_bytes());
    let mut n = 0;
    out[n] = FLAG;
    n += 1;
    for &b in &raw[..frame.len + 5] {
        if b == FLAG || b == ESC {
            out[n] = ESC;
            n += 1;
            out[n] = b ^ 0x20;
            n += 1;
        } else {
            out[n] = b;
            n += 1;
        }
    }
    out[n] = FLAG;
    n + 1
}

pub struct Decoder {
    raw: [u8; MAX_PAYLOAD + 5],
    len: usize,
    escaping: bool,
    started: bool,
}
impl Default for Decoder {
    fn default() -> Self {
        Self {
            raw: [0; MAX_PAYLOAD + 5],
            len: 0,
            escaping: false,
            started: false,
        }
    }
}
impl Decoder {
    pub fn push(&mut self, byte: u8) -> Option<Frame> {
        if byte == FLAG {
            let result = if self.started && !self.escaping {
                self.finish()
            } else {
                None
            };
            self.started = true;
            self.len = 0;
            self.escaping = false;
            return result;
        }
        if !self.started {
            return None;
        }
        if byte == ESC {
            self.escaping = true;
            return None;
        }
        let value = if self.escaping {
            self.escaping = false;
            byte ^ 0x20
        } else {
            byte
        };
        if self.len == self.raw.len() {
            self.started = false;
            return None;
        }
        self.raw[self.len] = value;
        self.len += 1;
        None
    }
    fn finish(&self) -> Option<Frame> {
        if self.len < 5 {
            return None;
        }
        let len = u16::from_le_bytes([self.raw[1], self.raw[2]]) as usize;
        if len > MAX_PAYLOAD || self.len != len + 5 {
            return None;
        }
        let got = u16::from_le_bytes([self.raw[len + 3], self.raw[len + 4]]);
        if got != crc16(&self.raw[..len + 3]) {
            return None;
        }
        Frame::new(self.raw[0], &self.raw[3..len + 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn round_trip(frame: Frame) -> Option<Frame> {
        let mut wire = [0; MAX_WIRE];
        let n = encode(&frame, &mut wire);
        let mut decoder = Decoder::default();
        wire[..n].iter().find_map(|b| decoder.push(*b))
    }
    #[test]
    fn audio_round_trip() {
        let mut audio = [128; 160];
        audio[3] = FLAG;
        audio[4] = ESC;
        let frame = Frame::new(AUDIO, &audio).unwrap();
        assert_eq!(round_trip(frame), Some(frame));
    }
    #[test]
    fn max_escaped_payload() {
        let frame = Frame::new(AUDIO, &[FLAG; MAX_PAYLOAD]).unwrap();
        assert_eq!(round_trip(frame), Some(frame));
    }
    #[test]
    fn corrupt_frame_resynchronizes() {
        let frame = Frame::new(MODEM, b"RING").unwrap();
        let mut wire = [0; MAX_WIRE];
        let n = encode(&frame, &mut wire);
        wire[3] ^= 1;
        let mut decoder = Decoder::default();
        assert!(wire[..n].iter().all(|b| decoder.push(*b).is_none()));
        let n = encode(&frame, &mut wire);
        assert_eq!(wire[..n].iter().find_map(|b| decoder.push(*b)), Some(frame));
    }
    #[test]
    fn bounds_payload() {
        assert!(Frame::new(AUDIO, &[0; 193]).is_none());
    }
}
