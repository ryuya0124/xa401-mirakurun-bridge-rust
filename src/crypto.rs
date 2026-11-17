//! DTCP public protocol constants and crypto, ported from pydtcpip (MIT).
//! Credentials are external and deliberately never implement Debug/Serialize.
use anyhow::{Result, bail, ensure};
use openssl::{
    bn::{BigNum, BigNumContext},
    ec::{EcGroup, EcKey, EcPoint},
    ecdsa::EcdsaSig,
    pkey::{Private, Public},
    sha::sha1,
    symm::{Cipher, Crypter, Mode},
};
use zeroize::Zeroizing;

pub fn group() -> Result<EcGroup> {
    let mut ctx = BigNumContext::new()?;
    let mut g = EcGroup::from_components(
        BigNum::from_hex_str("94A8E16AAD4F50EFE29B9D5C0C0B6D481B27DE53")?,
        BigNum::from_hex_str("94A8E16AAD4F50EFE29B9D5C0C0B6D481B27DE50")?,
        BigNum::from_hex_str("04FD918E5B0C4513FA3EF3033FE08F07C3B08C76D")?,
        &mut ctx,
    )?;
    let mut p = EcPoint::new(&g)?;
    let gx = BigNum::from_hex_str("078668913DEAB22759E587EB393D7DA46CE24B196")?;
    let gy = BigNum::from_hex_str("4D2BA996A7E3D9AF4D266AFE00443B95D6870606")?;
    p.set_affine_coordinates_gfp(&g, &gx, &gy, &mut ctx)?;
    g.set_generator(p, order()?, BigNum::from_u32(69)?)?;
    Ok(g)
}
fn order() -> Result<BigNum> {
    Ok(BigNum::from_hex_str(
        "02278C8AD247A46B5C53DE4CC9A0072488F028A3",
    )?)
}

pub fn point(g: &EcGroup, raw: &[u8]) -> Result<EcPoint> {
    ensure!(raw.len() == 40, "Invalid public point length");
    let mut ctx = BigNumContext::new()?;
    let mut p = EcPoint::new(g)?;
    let x = BigNum::from_slice(&raw[..20])?;
    let y = BigNum::from_slice(&raw[20..])?;
    p.set_affine_coordinates_gfp(g, &x, &y, &mut ctx)?;
    ensure!(
        !p.is_infinity(g) && p.is_on_curve(g, &mut ctx)?,
        "Invalid public point"
    );
    let mut check = EcPoint::new(g)?;
    let n = order()?;
    check.mul2(g, &p, &n, &mut ctx)?;
    ensure!(check.is_infinity(g), "Public point outside subgroup");
    Ok(p)
}
pub fn public_bytes(key: &EcKey<Private>) -> Result<Vec<u8>> {
    let mut x = BigNum::new()?;
    let mut y = BigNum::new()?;
    let mut ctx = BigNumContext::new()?;
    key.public_key()
        .affine_coordinates_gfp(key.group(), &mut x, &mut y, &mut ctx)?;
    Ok([x.to_vec_padded(20)?, y.to_vec_padded(20)?].concat())
}
pub fn verify(key: &EcKey<Public>, data: &[u8], signature: &[u8]) -> Result<()> {
    ensure!(signature.len() == 40, "Invalid signature size");
    let s = EcdsaSig::from_private_components(
        BigNum::from_slice(&signature[..20])?,
        BigNum::from_slice(&signature[20..])?,
    )?;
    ensure!(s.verify(&sha1(data), key)?, "Signature verification failed");
    Ok(())
}
pub fn authority(data: &[u8], signature: &[u8]) -> Result<()> {
    let g = group()?;
    for k in [
        "6e6e94c45dbedd467e42b789cce95cd19abcc2cc35d26bc0dff1dfeb2857088e052c5d9b2b339ad5",
        "23b8461648aa748babae9d9bd905c7ab40ea8efa743f1b3bb1fc17fbb952ad4f84f1f39a886a59b7",
    ] {
        let p = point(&g, &hex::decode(k)?)?;
        let key = EcKey::from_public_key(&g, &p)?;
        if verify(&key, data, signature).is_ok() {
            return Ok(());
        }
    }
    bail!("Authority signature verification failed")
}
pub fn certificate(raw: &[u8]) -> Result<EcKey<Public>> {
    ensure!(
        raw.len() == 88 && raw[0] == 1 && raw[1] >> 4 <= 1,
        "Invalid baseline certificate"
    );
    authority(&raw[..48], &raw[48..])?;
    let g = group()?;
    let p = point(&g, &raw[8..48])?;
    Ok(EcKey::from_public_key(&g, &p)?)
}
pub struct Credentials {
    pub certificate: Vec<u8>,
    pub key: EcKey<Private>,
}
impl Credentials {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        ensure!(
            std::fs::metadata(path)?.len() <= 4096,
            "Credentials file too large"
        );
        let text = Zeroizing::new(std::fs::read_to_string(path)?);
        let mut cert = None;
        let mut private = None;
        for line in text.lines() {
            if let Some((name, value)) = line.trim().split_once('=') {
                let value = value.trim().trim_matches(['\'', '"']);
                match name.trim() {
                    "PYDTCPIP_CERTIFICATE" => {
                        cert = Some(
                            hex::decode(value)
                                .map_err(|_| anyhow::anyhow!("Invalid credentials encoding"))?,
                        )
                    }
                    "PYDTCPIP_PRIVATE_KEY" => {
                        private =
                            Some(Zeroizing::new(hex::decode(value).map_err(|_| {
                                anyhow::anyhow!("Invalid credentials encoding")
                            })?))
                    }
                    _ => (),
                }
            }
        }
        let cert = cert.ok_or_else(|| anyhow::anyhow!("Missing certificate"))?;
        let public = certificate(&cert)?;
        let private = private.ok_or_else(|| anyhow::anyhow!("Missing private key"))?;
        ensure!(private.len() == 20, "Invalid credential size");
        let g = group()?;
        let mut ctx = BigNumContext::new()?;
        let mut scalar = BigNum::from_slice(&private)?;
        scalar.set_const_time();
        ensure!(
            scalar.num_bits() > 0 && scalar < order()?,
            "Invalid credential range"
        );
        let mut p = EcPoint::new(&g)?;
        p.mul_generator2(&g, &scalar, &mut ctx)?;
        ensure!(
            p.eq(&g, public.public_key(), &mut ctx)?,
            "Credential public key mismatch"
        );
        let key = EcKey::from_private_components(&g, &scalar, &p)?;
        key.check_key()?;
        Ok(Self {
            certificate: cert,
            key,
        })
    }
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>> {
        let signature = EcdsaSig::sign(&sha1(data), &self.key)?;
        Ok([
            signature.r().to_vec_padded(20)?,
            signature.s().to_vec_padded(20)?,
        ]
        .concat())
    }
}
pub fn shared(key: &EcKey<Private>, peer: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let g = group()?;
    let p = point(&g, peer)?;
    let mut q = EcPoint::new(&g)?;
    let mut ctx = BigNumContext::new()?;
    q.mul2(&g, &p, key.private_key(), &mut ctx)?;
    ensure!(!q.is_infinity(&g), "Invalid shared point");
    let mut x = BigNum::new()?;
    let mut y = BigNum::new()?;
    q.affine_coordinates_gfp(&g, &mut x, &mut y, &mut ctx)?;
    let raw = Zeroizing::new(x.to_vec_padded(20)?);
    Ok(Zeroizing::new(raw[8..].to_vec()))
}
fn aes(cipher: Cipher, mode: Mode, key: &[u8], iv: Option<&[u8]>, input: &[u8]) -> Result<Vec<u8>> {
    let mut c = Crypter::new(cipher, mode, key, iv)?;
    c.pad(false);
    let mut out = vec![0; input.len() + cipher.block_size()];
    let n = c.update(input, &mut out)?;
    let tail = c.finalize(&mut out[n..])?;
    out.truncate(n + tail);
    Ok(out)
}
pub fn decrypt(exchange: &[u8], mode: u8, nonce: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        exchange.len() == 12 && nonce.len() == 8,
        "Invalid content key input"
    );
    let constant = match mode {
        2 => "de665adec0156112cf2628eb",
        4 => "45b146a0611a501ac32bfad4",
        6 => "e6ed3e3078ff0e5b3a273ccb",
        8 => "7c0d87c76c195662f98c14e6",
        10 => "28b522c7cdb22d490ef8e3d4",
        12 => "a3806f971635c1796deaed24",
        _ => bail!("Unsupported encryption mode"),
    };
    let material = Zeroizing::new([exchange, &hex::decode(constant)?, nonce].concat());
    let mut key = Zeroizing::new(aes(
        Cipher::aes_128_ecb(),
        Mode::Encrypt,
        &material[..16],
        None,
        &material[16..],
    )?);
    for (a, b) in key.iter_mut().zip(&material[16..]) {
        *a ^= b;
    }
    let iv = Zeroizing::new(aes(
        Cipher::aes_128_ecb(),
        Mode::Encrypt,
        &key,
        None,
        &[&hex::decode("95dc3a449028eb3c")?, nonce].concat(),
    )?);
    aes(Cipher::aes_128_cbc(), Mode::Decrypt, &key, Some(&iv), data)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_python_content_vector() {
        // Public artificial vector produced by pinned pydtcpip, no device secrets.
        let exchange = (0..12).collect::<Vec<u8>>();
        let nonce = (0..8).collect::<Vec<u8>>();
        let encrypted=hex::decode("43d1e7543d010ef947a3aa1cb36ef8f6012f085c4932d716307833d52b4e7a9beb726ceaaba80d6a502cf22dd5f28fe96ce2fd6ba1d50dabe8ff97b5da1cdbc8").unwrap();
        assert_eq!(
            decrypt(&exchange, 4, &nonce, &encrypted).unwrap(),
            (0..64).collect::<Vec<u8>>()
        );
        assert!(decrypt(&exchange, 0, &nonce, &encrypted).is_err());
    }
    #[test]
    fn shared_points_and_signatures() {
        let g = group().unwrap();
        let a = EcKey::generate(&g).unwrap();
        let b = EcKey::generate(&g).unwrap();
        assert_eq!(
            shared(&a, &public_bytes(&b).unwrap()).unwrap().as_slice(),
            shared(&b, &public_bytes(&a).unwrap()).unwrap().as_slice()
        );
        let public = EcKey::from_public_key(&g, a.public_key()).unwrap();
        let c = Credentials {
            certificate: vec![],
            key: a,
        };
        let sig = c.sign(b"artificial interoperability test").unwrap();
        assert!(verify(&public, b"artificial interoperability test", &sig).is_ok());
        assert!(verify(&public, b"modified", &sig).is_err());
        assert!(point(&g, &[0; 40]).is_err());
        assert!(certificate(&[0; 88]).is_err());
    }
    #[test]
    fn linked_openssl_is_current() {
        assert!(
            openssl::version::number() >= 0x40100000,
            "Build with the pinned OpenSSL 4.1 library: {}",
            openssl::version::version()
        );
    }
}
