//! Source-bound full authentication only. No Move, provisioning or fallback.
use crate::{
    crypto::{self, Credentials},
    srm::Srm,
};
use anyhow::{Result, bail, ensure};
use openssl::{ec::EcKey, memcmp, rand::rand_bytes, sha::sha1};
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpSocket, TcpStream},
    time::{Duration, timeout},
};
use zeroize::Zeroizing;
pub struct Exchange {
    pub value: Zeroizing<Vec<u8>>,
    pub label: u8,
}
struct Frame {
    h: [u8; 11],
    data: Vec<u8>,
}
impl Frame {
    fn request(cmd: u8, data: Vec<u8>) -> Self {
        Self {
            h: [1, 0, 0, 0, 0, cmd, 4, 8, 1, 0, 15],
            data,
        }
    }
    fn accepted(&self, data: Vec<u8>) -> Self {
        let mut h = self.h;
        h[3] = (h[3] & 240) | 9;
        h[10] &= 240;
        Self { h, data }
    }
    fn control(&self, cmd: u8, len: usize) -> bool {
        self.h[3] & 15 == 0 && self.h[5] == cmd && self.data.len() == len
    }
    fn ok(&self, cmd: u8) -> bool {
        self.h[3] & 15 == 9 && self.h[10] & 15 == 0 && self.h[5] == cmd
    }
    async fn send(&self, s: &mut TcpStream) -> Result<()> {
        ensure!(self.data.len() <= 65527, "Control payload too large");
        let mut h = self.h;
        h[1..3].copy_from_slice(&((self.data.len() + 8) as u16).to_be_bytes());
        let mut data = Vec::with_capacity(11 + self.data.len());
        data.extend(h);
        data.extend(&self.data);
        s.write_all(&data).await?;
        Ok(())
    }
    async fn read(s: &mut TcpStream) -> Result<Self> {
        let mut h = [0u8; 11];
        s.read_exact(&mut h[..3]).await?;
        let len = u16::from_be_bytes([h[1], h[2]]) as usize;
        ensure!(h[0] == 1 && len >= 8, "Invalid control frame");
        s.read_exact(&mut h[3..]).await?;
        ensure!(
            h[3] & 15 == 0 || (h[3] & 15 == 9 && h[10] & 15 == 0),
            "Authentication rejected"
        );
        let mut data = vec![0; len - 8];
        s.read_exact(&mut data).await?;
        Ok(Self { h, data })
    }
}
fn rtt_digest(base: &[u8; 20], counter: u32) -> [u8; 20] {
    let mut value = *base;
    let mut carry = counter;
    for byte in value.iter_mut().rev() {
        carry += *byte as u32;
        *byte = carry as u8;
        carry >>= 8;
    }
    sha1(&value)
}
pub async fn authenticate(
    source: Ipv4Addr,
    host: Ipv4Addr,
    port: u16,
    credentials: &Credentials,
    srm_path: &Path,
) -> Result<Exchange> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((source, 0)))?;
    socket2::SockRef::from(&socket).set_ttl_v4(3)?;
    let mut stream = timeout(
        Duration::from_secs(15),
        socket.connect(SocketAddr::from((host, port))),
    )
    .await??;
    stream.set_ttl(3)?;
    stream.set_nodelay(true)?;
    timeout(
        Duration::from_secs(15),
        run(&mut stream, credentials, srm_path),
    )
    .await?
}
async fn run(s: &mut TcpStream, c: &Credentials, srm_path: &Path) -> Result<Exchange> {
    let mut srm = Srm::load(srm_path)?;
    srm.check(&c.certificate)?;
    let mut nonce = [0; 16];
    rand_bytes(&mut nonce)?;
    let g = crypto::group()?;
    let ephemeral = EcKey::generate(&g)?;
    Frame::request(1, [nonce.to_vec(), c.certificate.clone()].concat())
        .send(s)
        .await?;
    ensure!(Frame::read(s).await?.ok(1), "CHALLENGE not accepted");
    let f = Frame::read(s).await?;
    ensure!(f.control(1, 104), "Invalid CHALLENGE");
    let peer_nonce = f.data[..16].to_vec();
    let peer_cert = f.data[16..].to_vec();
    let peer = crypto::certificate(&peer_cert)?;
    srm.check(&peer_cert)?;
    f.accepted(vec![]).send(s).await?;
    let f = Frame::read(s).await?;
    ensure!(f.control(2, 83), "Invalid RESPONSE");
    crypto::verify(
        &peer,
        &[nonce.to_vec(), f.data[..43].to_vec()].concat(),
        &f.data[43..],
    )?;
    let peer_version = u16::from_be_bytes([f.data[40], f.data[41]]);
    let peer_gen = f.data[42] & 15;
    let peer_max = peer_cert[1] >> 4;
    ensure!(peer_gen <= peer_max, "Invalid peer SRM generation");
    let auth = crypto::shared(&ephemeral, &f.data[..40])?;
    f.accepted(vec![]).send(s).await?;
    let mut data = crypto::public_bytes(&ephemeral)?;
    data.extend(srm.version.to_be_bytes());
    data.push(srm.generation);
    let signature = c.sign(&[peer_nonce, data.clone()].concat())?;
    data.extend(signature);
    Frame::request(2, data).send(s).await?;
    ensure!(Frame::read(s).await?.ok(2), "RESPONSE not accepted");
    let mut need_srm = peer_version > srm.version
        || (c.certificate[1] >> 4 > 0 && peer_version == srm.version && peer_gen > srm.generation);
    let base = sha1(&[auth.as_slice(), auth.as_slice()].concat());
    let mut expected = None;
    let mut reply = Vec::new();
    let mut verify = Vec::new();
    let mut received = None;
    let mut exchange = None;
    for _ in 0..64 {
        if exchange.is_some() && !need_srm {
            return Ok(exchange.unwrap());
        }
        let f = Frame::read(s).await?;
        if f.h[3] & 15 != 0 {
            ensure!(f.ok(0x91), "Unexpected authentication response");
            continue;
        }
        match f.h[5] {
            0x91 => {
                f.accepted(vec![]).send(s).await?;
                Frame::request(0x91, vec![]).send(s).await?;
            }
            0x11 => {
                ensure!(f.data.len() == 2, "Invalid RTT counter");
                let count = u16::from_be_bytes([f.data[0], f.data[1]]) as u32;
                let digest = rtt_digest(&base, count);
                expected = Some(digest[..10].to_vec());
                reply = digest[10..].to_vec();
                verify = rtt_digest(&base, count + 1)[..10].to_vec();
                f.accepted(f.data.clone()).send(s).await?;
            }
            0x12 => {
                ensure!(
                    expected.is_some() && f.data.len() == 10,
                    "RTT setup required"
                );
                received = Some(f.data.clone());
                f.accepted(reply.clone()).send(s).await?;
            }
            0x92 => {
                ensure!(
                    matches!((&expected,&received),(Some(a),Some(b)) if memcmp::eq(a,b)),
                    "RTT verification failed"
                );
                f.accepted(verify.clone()).send(s).await?;
            }
            3 => {
                ensure!(f.data.len() == 14, "Invalid exchange key");
                exchange = Some(Exchange {
                    value: Zeroizing::new(
                        auth.iter().zip(&f.data[2..]).map(|(a, b)| a ^ b).collect(),
                    ),
                    label: f.data[0],
                });
                f.accepted(vec![]).send(s).await?;
                if srm.version > peer_version
                    || (srm.version == peer_version
                        && peer_gen < peer_max
                        && srm.generation > peer_gen)
                {
                    let len = if srm.generation > peer_max {
                        4 + u16::from_be_bytes([srm.data[4], srm.data[5]]) as usize
                    } else {
                        srm.data.len()
                    };
                    let mut update = Frame::request(4, srm.data[..len].to_vec());
                    update.h[7] = 0;
                    update.send(s).await?;
                    ensure!(Frame::read(s).await?.ok(4), "SRM update rejected");
                }
            }
            4 => {
                let new = Srm::parse(&f.data)?;
                if new.newer(&srm) {
                    new.save(srm_path)?;
                    srm = new;
                }
                srm.check(&peer_cert)?;
                srm.check(&c.certificate)?;
                f.accepted(vec![]).send(s).await?;
                need_srm = false;
            }
            _ => bail!("Unsupported authentication command"),
        }
    }
    bail!("Authentication frame limit exceeded")
}
