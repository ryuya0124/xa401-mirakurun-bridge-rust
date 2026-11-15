//! Signature-verified revocation state; never discard an invalid stored SRM.
use crate::crypto;
use anyhow::{Result, ensure};
#[derive(Default, Clone)]
pub struct Srm {
    pub data: Vec<u8>,
    pub version: u16,
    pub generation: u8,
    ranges: Vec<(u64, u64)>,
}
impl Srm {
    pub fn parse(data: &[u8]) -> Result<Self> {
        ensure!(
            (46..=1024).contains(&data.len()) && data[0] <= 1 && data[1] == 0,
            "Invalid SRM header"
        );
        let mut pos = 4;
        let mut generation = 0;
        let mut ranges = Vec::new();
        while pos < data.len() {
            ensure!(
                generation <= data[0] && pos + 2 <= data.len(),
                "Invalid SRM generation"
            );
            let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            let end = pos + len;
            ensure!(
                (42..=if generation == 0 { 124 } else { 978 }).contains(&len) && end <= data.len(),
                "Invalid SRM size"
            );
            crypto::authority(&data[..end - 40], &data[end - 40..end])?;
            pos += 2;
            while pos < end - 40 {
                let tag = data[pos];
                pos += 1;
                let kind = tag >> 5;
                let count = (tag & 31) as usize;
                ensure!(kind <= 1, "Invalid SRM entry");
                let width = if kind == 0 { 5 } else { 7 };
                ensure!(pos + count * width <= end - 40, "Truncated SRM entry");
                for _ in 0..count {
                    let first = data[pos..pos + 5]
                        .iter()
                        .fold(0u64, |a, b| a * 256 + *b as u64);
                    let span = if kind == 1 {
                        u16::from_be_bytes([data[pos + 5], data[pos + 6]]) as u64
                    } else {
                        0
                    };
                    ranges.push((first, first + span));
                    pos += width;
                }
            }
            pos = end;
            generation += 1;
        }
        Ok(Self {
            data: data.to_vec(),
            version: u16::from_be_bytes([data[2], data[3]]),
            generation: generation - 1,
            ranges,
        })
    }
    pub fn load(path: &std::path::Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(data) => Self::parse(&data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn newer(&self, other: &Self) -> bool {
        (self.version, self.data.len()) > (other.version, other.data.len())
    }
    pub fn check(&self, cert: &[u8]) -> Result<()> {
        ensure!(cert.len() == 88, "Invalid certificate length");
        let id = cert[3..8].iter().fold(0u64, |a, b| a * 256 + *b as u64);
        ensure!(
            !self.ranges.iter().any(|(a, b)| *a <= id && id <= *b),
            "Revoked certificate"
        );
        Ok(())
    }
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &self.data)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}
