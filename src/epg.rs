//! Observed SIT only; no invented future EPG or copy-control classification.
use serde_json::{Map, Value, json};
pub struct Parser {
    buffer: Vec<u8>,
    cc: Option<u8>,
    last_packet: Vec<u8>,
    last_section: Vec<u8>,
    expected: Value,
}
fn u16be(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn crc(data: &[u8]) -> u32 {
    let mut crc = 0xffffffffu32;
    for b in data {
        crc ^= (*b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x80000000 != 0 {
                (crc << 1) ^ 0x04c11db7
            } else {
                crc << 1
            };
        }
    }
    crc
}
fn text(b: &[u8]) -> String {
    arib::text::Decoder::default().decode(b).trim().to_string()
}
fn bcd(b: u8) -> Option<i64> {
    if b >> 4 <= 9 && b & 15 <= 9 {
        Some((b >> 4) as i64 * 10 + (b & 15) as i64)
    } else {
        None
    }
}
fn seconds(b: &[u8]) -> Option<i64> {
    if b.len() < 3 {
        return None;
    }
    let h = bcd(b[0])?;
    let m = bcd(b[1])?;
    let s = bcd(b[2])?;
    if m >= 60 || s >= 60 {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}
fn descriptors(mut bytes: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut result = vec![];
    while !bytes.is_empty() {
        if bytes.len() < 2 || bytes.len() < 2 + bytes[1] as usize {
            return None;
        }
        let n = bytes[1] as usize;
        result.push((bytes[0], &bytes[2..2 + n]));
        bytes = &bytes[2 + n..];
    }
    Some(result)
}
impl Parser {
    pub fn new(expected: Value) -> Self {
        Self {
            buffer: vec![],
            cc: None,
            last_packet: vec![],
            last_section: vec![],
            expected,
        }
    }
    fn drain(&mut self) -> Vec<Value> {
        let mut result = vec![];
        while self.buffer.len() >= 3 {
            if self.buffer[0] == 255 {
                self.buffer.clear();
                break;
            }
            let n = 3 + (((self.buffer[1] & 15) as usize) << 8) + self.buffer[2] as usize;
            if !(12..=4096).contains(&n) {
                self.buffer.clear();
                break;
            }
            if self.buffer.len() < n {
                break;
            }
            let section = self.buffer.drain(..n).collect::<Vec<_>>();
            if section[0] != 0x7f
                || section[1] & 128 == 0
                || section[5] & 1 == 0
                || crc(&section) != 0
                || section == self.last_section
            {
                continue;
            }
            if let Some(items) = self.section(&section) {
                result.extend(items);
                self.last_section = section;
            }
        }
        result
    }
    fn section(&self, s: &[u8]) -> Option<Vec<Value>> {
        if s.len() < 14 {
            return None;
        }
        let mut pos = 10 + ((u16be(&s[8..10]) & 4095) as usize);
        let mut out = vec![];
        while pos + 4 <= s.len() - 4 {
            let sid = u16be(&s[pos..]);
            let n = (u16be(&s[pos + 2..]) & 4095) as usize;
            pos += 4;
            if pos + n > s.len() - 4 {
                return None;
            }
            let desc = descriptors(&s[pos..pos + n])?;
            pos += n;
            let ids = desc.iter().find(|(tag, b)| *tag == 0x85 && b.len() >= 7)?.1;
            let nid = u16be(ids);
            let tsid = u16be(&ids[2..]);
            let eid = u16be(&ids[4..]);
            if self.expected["networkId"].as_u64() != Some(nid as u64)
                || self.expected["transportStreamId"].as_u64() != Some(tsid as u64)
                || self.expected["serviceId"].as_u64() != Some(sid as u64)
            {
                continue;
            }
            let clock = desc
                .iter()
                .find(|(tag, b)| *tag == 0xc3 && b.len() >= 13)?
                .1;
            if clock[1..6] == [255; 5] {
                continue;
            }
            let delta =
                seconds(&clock[9..12]).unwrap_or(0) * if clock[12] & 4 == 0 { 1 } else { -1 };
            let start = ((u16be(&clock[1..]) as i64 - 40587) * 86400 + seconds(&clock[3..6])?
                - 32400
                + delta)
                * 1000;
            let duration = seconds(&clock[6..9])? * 1000;
            if !(946684800000..=4102444800000).contains(&start)
                || duration <= 0
                || duration > 7 * 86400000
            {
                continue;
            }
            let short = desc.iter().find(|(tag, b)| *tag == 0x4d && b.len() >= 5)?.1;
            let len = short[3] as usize;
            if short.len() < 5 + len {
                return None;
            }
            let title = text(&short[4..4 + len]);
            let tlen = short[4 + len] as usize;
            if short.len() < 5 + len + tlen {
                return None;
            }
            let description = text(&short[5 + len..5 + len + tlen]);
            if title.is_empty() {
                continue;
            }
            let mut extended = Map::new();
            let mut last_key = String::new();
            let mut extended_desc = desc
                .iter()
                .filter(|(tag, b)| *tag == 0x4e && b.len() >= 5)
                .collect::<Vec<_>>();
            extended_desc.sort_by_key(|(_, b)| b[0] >> 4);
            for (_, body) in extended_desc {
                let end = 5 + body[4] as usize;
                if end > body.len() {
                    return None;
                }
                let mut p = 5;
                while p < end {
                    let kn = body[p] as usize;
                    p += 1;
                    if p + kn >= end {
                        return None;
                    }
                    let key = text(&body[p..p + kn]);
                    p += kn;
                    let vn = body[p] as usize;
                    p += 1;
                    if p + vn > end {
                        return None;
                    }
                    let value = text(&body[p..p + vn]);
                    p += vn;
                    let key = if key.is_empty() && !last_key.is_empty() {
                        last_key.clone()
                    } else {
                        key
                    };
                    let entry = extended.entry(key.clone()).or_insert(json!(""));
                    let mut previous = entry.as_str().unwrap_or("").to_string();
                    previous.push_str(&value);
                    *entry = json!(previous);
                    last_key = key;
                }
            }
            let mut genres = vec![];
            for (_, b) in desc.iter().filter(|(tag, _)| *tag == 0x54) {
                for pair in b.chunks_exact(2) {
                    genres.push(json!({"lv1":pair[0]>>4,"lv2":pair[0]&15,"un1":pair[1]>>4,"un2":pair[1]&15}));
                }
            }
            let mut program = json!({"id":nid as u64*10000000000+sid as u64*100000+eid as u64,"networkId":nid,"serviceId":sid,"eventId":eid,"startAt":start,"duration":duration,"name":title,"description":description,"extended":extended,"genres":genres,"isFree":!desc.iter().any(|(t,_)|*t==9),"xa401MetadataSource":"SIT"});
            if desc
                .iter()
                .any(|(t, b)| *t == 0x50 && b.len() >= 2 && b[0] & 15 == 1 && b[1] & 240 == 0xb0)
            {
                program["video"] = json!({"streamContent":1,"componentType":desc.iter().find(|(t,b)|*t==0x50&&b.len()>=2).unwrap().1[1],"type":"mpeg2","resolution":"1080i"});
            }
            out.push(program);
        }
        Some(out)
    }
    pub fn feed(&mut self, data: &[u8]) -> Vec<Value> {
        let mut result = vec![];
        for p in data.chunks_exact(188) {
            if p[0] != 0x47 || (((p[1] & 31) as u16) << 8) | p[2] as u16 != 31 {
                continue;
            }
            if p[1] & 128 != 0 || p[3] & 192 != 0 {
                self.buffer.clear();
                self.cc = None;
                continue;
            }
            if p[3] & 16 == 0 {
                continue;
            }
            let mut pos = 4;
            if p[3] & 32 != 0 {
                if p[4] > 0 && p[5] & 128 != 0 {
                    self.buffer.clear();
                    self.cc = None
                }
                pos += 1 + p[4] as usize;
            }
            if pos >= 188 {
                continue;
            }
            let cc = p[3] & 15;
            if self.cc == Some(cc) && self.last_packet == p {
                continue;
            }
            if self.cc.is_some_and(|n| cc != (n + 1) % 16) {
                self.buffer.clear()
            }
            self.cc = Some(cc);
            self.last_packet = p.to_vec();
            let payload = &p[pos..];
            if p[1] & 64 != 0 {
                let pointer = payload[0] as usize;
                if pointer >= payload.len() {
                    self.buffer.clear();
                    continue;
                }
                if !self.buffer.is_empty() {
                    self.buffer.extend(&payload[1..1 + pointer]);
                    result.extend(self.drain());
                }
                self.buffer = payload[1 + pointer..].to_vec();
            } else if !self.buffer.is_empty() {
                self.buffer.extend(payload)
            }
            result.extend(self.drain());
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_reference() {
        assert_eq!(crc(b"123456789"), 0x0376e6e7);
    }
    #[test]
    fn invalid_bcd() {
        assert_eq!(seconds(&[0x12, 0x34, 0x56]), Some(45296));
        assert!(seconds(&[0x12, 0x60, 0]).is_none());
    }
}
