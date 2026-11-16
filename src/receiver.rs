use crate::{
    ake,
    crypto::{self, Credentials},
    device::{self, Resource},
};
use anyhow::{Result, ensure};
use bytes::{Buf, Bytes, BytesMut};
use std::{net::Ipv4Addr, path::Path};
use tokio::sync::mpsc;
pub struct Normalizer {
    buffer: BytesMut,
    stride: usize,
    offset: usize,
}
impl Normalizer {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::new(),
            stride: 0,
            offset: 0,
        }
    }
    pub fn feed(&mut self, data: &[u8]) -> Result<Bytes> {
        self.buffer.extend_from_slice(data);
        if self.stride == 0 {
            if self.buffer.len() < 192 * 6 {
                return Ok(Bytes::new());
            }
            for (stride, offset) in [(192, 4), (188, 0)] {
                if (0..6).all(|i| self.buffer[offset + i * stride] == 0x47) {
                    self.stride = stride;
                    self.offset = offset;
                    break;
                }
            }
            ensure!(self.stride != 0, "Decrypted data is not aligned MPEG-TS");
        }
        let count = self.buffer.len() / self.stride;
        for i in 0..count {
            let start = i * self.stride + self.offset;
            ensure!(self.buffer[start] == 0x47, "MPEG-TS sync lost");
        }
        if self.stride == 188 {
            return Ok(self.buffer.split_to(count * 188).freeze());
        }
        let mut out = BytesMut::with_capacity(count * 188);
        for i in 0..count {
            let start = i * self.stride + self.offset;
            out.extend_from_slice(&self.buffer[start..start + 188]);
        }
        self.buffer.advance(count * self.stride);
        Ok(out.freeze())
    }
}
fn endpoint(response: &reqwest::Response, host: Ipv4Addr) -> Result<u16> {
    ensure!(response.status().is_success(), "Media HTTP request failed");
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|s| s.to_str().ok())
        .unwrap_or("");
    let mut parts = mime.split(';');
    ensure!(
        parts
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/x-dtcp1"),
        "Expected protected DTCP media"
    );
    let mut address = None;
    let mut port = None;
    for p in parts {
        if let Some((k, v)) = p.trim().split_once('=') {
            let v = v.trim().trim_matches('"');
            if k.eq_ignore_ascii_case("DTCP1HOST") {
                address = Some(v.parse::<Ipv4Addr>()?)
            }
            if k.eq_ignore_ascii_case("DTCP1PORT") {
                port = Some(v.parse::<u16>()?)
            }
        }
    }
    ensure!(
        address == Some(host) && port.is_some_and(|p| p > 0),
        "AKE destination left verified device"
    );
    Ok(port.unwrap())
}
pub async fn receive(
    source: Ipv4Addr,
    host: Ipv4Addr,
    resource: &Resource,
    credentials: &Credentials,
    srm: &Path,
    position: f64,
    tx: mpsc::Sender<Bytes>,
) -> Result<()> {
    receive_locked(source, host, resource, credentials, srm, position, tx, None).await
}
pub async fn receive_locked(
    source: Ipv4Addr,
    host: Ipv4Addr,
    resource: &Resource,
    credentials: &Credentials,
    srm: &Path,
    position: f64,
    tx: mpsc::Sender<Bytes>,
    ake_lock: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
) -> Result<()> {
    device::lan_url(&resource.url, Some(host))?;
    ensure!(
        position.is_finite() && position >= 0.,
        "Invalid seek position"
    );
    let http = device::client(source)?;
    let request = |method| {
        let r = http
            .request(method, &resource.url)
            .header("transferMode.dlna.org", "Streaming")
            .header("getcontentFeatures.dlna.org", "1")
            .header("Accept-Encoding", "identity");
        if position > 0. {
            r.header("TimeSeekRange.dlna.org", format!("npt={:.3}-", position))
        } else {
            r
        }
    };
    let head = request(reqwest::Method::HEAD).send().await?;
    let port = endpoint(&head, host)?;
    drop(head);
    let lock = if let Some(ref mutex) = ake_lock {
        Some(mutex.lock().await)
    } else {
        None
    };
    let exchange = ake::authenticate(source, host, port, credentials, srm).await?;
    drop(lock);
    let mut response = request(reqwest::Method::GET).send().await?;
    ensure!(endpoint(&response, host)? == port, "AKE endpoint changed");
    if position > 0. {
        ensure!(
            response.headers().contains_key("TimeSeekRange.dlna.org"),
            "Seek was not acknowledged"
        );
    }
    let mut buffer = BytesMut::new();
    let mut normalizer = Normalizer::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            buffer.len() + chunk.len() <= 16 * 1024 * 1024,
            "Receive buffer limit exceeded"
        );
        buffer.extend(chunk);
        loop {
            if buffer.len() < 14 {
                break;
            }
            ensure!(
                buffer[0] >> 4 == 0 && buffer[1] == exchange.label,
                "PCP cipher/type/key label mismatch"
            );
            let len = u32::from_be_bytes(buffer[10..14].try_into()?) as usize;
            ensure!(len <= 8 * 1024 * 1024, "PCP exceeds 8MiB");
            let padded = (len + 15) / 16 * 16;
            if buffer.len() < 14 + padded {
                break;
            }
            let mut raw = crypto::decrypt(
                &exchange.value,
                buffer[0] & 15,
                &buffer[2..10],
                &buffer[14..14 + padded],
            )?;
            raw.truncate(len);
            buffer.advance(14 + padded);
            let ts = normalizer.feed(&raw)?;
            if !ts.is_empty() && tx.send(ts).await.is_err() {
                return Ok(());
            }
        }
    }
    ensure!(buffer.is_empty(), "Truncated protected stream");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packet_boundaries_survive_network_splits() {
        for stride in [188, 192] {
            let mut input = vec![0; stride * 19];
            for i in 0..19 {
                input[i * stride + stride - 188] = 0x47;
            }
            for split in [1, 187, 188, 191, 192, 997, 4096] {
                let mut n = Normalizer::new();
                let mut out = vec![];
                for chunk in input.chunks(split) {
                    out.extend(n.feed(chunk).unwrap());
                }
                assert_eq!(out.len(), 188 * 19);
                assert!(out.chunks(188).all(|p| p[0] == 0x47));
            }
        }
    }
    #[test]
    fn corrupt_plaintext_rejected() {
        assert!(Normalizer::new().feed(&[0; 192 * 6]).is_err());
    }
    #[test]
    fn payload_and_held_chunks_survive_normalization() {
        for stride in [188, 192] {
            let mut input = vec![0; stride * 19];
            let mut expected = vec![];
            for (i, packet) in input.chunks_mut(stride).enumerate() {
                let ts = &mut packet[stride - 188..];
                ts.fill(i as u8);
                ts[0] = 0x47;
                expected.extend_from_slice(ts);
            }
            let mut n = Normalizer::new();
            // Retain each Bytes while subsequent feeds reuse the mutable buffer.
            let chunks = input
                .chunks(1301)
                .map(|s| n.feed(s).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(chunks.concat(), expected);
            let mut bad = vec![0; stride];
            bad[stride - 188] = 0x46;
            assert!(n.feed(&bad).is_err());
        }
    }
}
