
use super::consts::*;
use super::context::PgpContext;
use super::packet::read_fixed;
use ::num_bigint::BigUint;
use ::num_traits::{One, Zero};
use ::pg_strong_random::pg_strong_random;

#[derive(Clone)]
pub struct Mpi {
    pub bits: usize,
    pub data: Vec<u8>,
}

impl Mpi {
    pub fn from_bytes(data: Vec<u8>, bits: usize) -> Mpi {
        Mpi { bits, data }
    }

    pub fn from_biguint(n: &BigUint) -> Mpi {
        let data = if n.is_zero() {
            Vec::new()
        } else {
            n.to_bytes_be()
        };
        Mpi {
            bits: n.bits() as usize,
            data,
        }
    }

    pub fn to_biguint(&self) -> BigUint {
        BigUint::from_bytes_be(&self.data)
    }

    pub fn nbytes(&self) -> usize {
        self.data.len()
    }
}

/// pgp-mpi.c:80 pgp_mpi_read: two pullf_read_fixed calls (header, body).
pub fn read_mpi(data: &[u8], pos: &mut usize, ctx: &mut PgpContext) -> Result<Mpi, String> {
    let hdr = read_fixed(data, *pos, 2, ctx)?;
    let bits = ((hdr[0] as usize) << 8) | hdr[1] as usize;
    *pos += 2;
    let nbytes = (bits + 7) / 8;
    let value = read_fixed(data, *pos, nbytes, ctx)?.to_vec();
    *pos += nbytes;
    Ok(Mpi::from_bytes(value, bits))
}

pub fn write_mpi(dst: &mut Vec<u8>, m: &Mpi) {
    dst.push((m.bits >> 8) as u8);
    dst.push((m.bits & 0xFF) as u8);
    dst.extend_from_slice(&m.data);
}

pub fn mpi_cksum(mut cksum: u32, m: &Mpi) -> u32 {
    cksum += (m.bits >> 8) as u32;
    cksum += (m.bits & 0xFF) as u32;
    for &b in &m.data {
        cksum += b as u32;
    }
    cksum & 0xFFFF
}

fn decide_k_bits(p_bits: usize) -> usize {
    if p_bits <= 5120 {
        p_bits / 10 + 160
    } else {
        (p_bits / 8 + 200) * 3 / 2
    }
}

fn rand_bits(bits: usize) -> Result<BigUint, MathError> {
    if bits == 0 {
        return Ok(BigUint::zero());
    }
    let nbytes = (bits + 7) / 8;
    let mut buf = vec![0u8; nbytes];
    // pgp-mpi-openssl.c:123: a BN_rand failure leaves res = PXE_PGP_MATH_FAILED.
    if !pg_strong_random(&mut buf) {
        return Err(math_failed());
    }
    let excess = nbytes * 8 - bits;
    buf[0] &= 0xFF >> excess;
    buf[0] |= 0x80 >> excess;
    Ok(BigUint::from_bytes_be(&buf))
}

/// PXE_PGP_MATH_FAILED from pgp-mpi-openssl.c; `dbg` is the px_debug line
/// mpi_to_bn emits when an MPI's declared bit count is not its value's.
pub struct MathError {
    pub dbg: Option<String>,
}

impl MathError {
    pub fn report(self, ctx: &mut super::context::PgpContext) -> String {
        if let Some(d) = &self.dbg {
            ctx.dbg(d);
        }
        MATH_FAILED.to_string()
    }
}

impl From<MathError> for String {
    fn from(_: MathError) -> String {
        MATH_FAILED.to_string()
    }
}

fn math_failed() -> MathError {
    MathError { dbg: None }
}

/// pgp-mpi-openssl.c:45 mpi_to_bn: BN_num_bits(bn) must equal the declared
/// bit count, or the conversion fails.
fn to_bn(m: &Mpi) -> Result<BigUint, MathError> {
    let bn = m.to_biguint();
    let bits = bn.bits() as usize;
    if bits != m.bits {
        return Err(MathError {
            dbg: Some(format!("mpi_to_bn: bignum conversion failed: mpi={}, bn={bits}", m.bits)),
        });
    }
    Ok(bn)
}

/// BN_mod_exp: a zero modulus is a division by zero, PXE_PGP_MATH_FAILED
/// for every caller in pgp-mpi-openssl.c.
fn mod_exp(base: &BigUint, exp: &BigUint, modulus: &BigUint) -> Result<BigUint, MathError> {
    if modulus.is_zero() {
        return Err(math_failed());
    }
    Ok(base.modpow(exp, modulus))
}

pub fn rsa_encrypt(n: &Mpi, e: &Mpi, m: &Mpi) -> Result<Mpi, MathError> {
    let (n, e, m) = (to_bn(n)?, to_bn(e)?, to_bn(m)?);
    let c = mod_exp(&m, &e, &n)?;
    Ok(Mpi::from_biguint(&c))
}

pub fn rsa_decrypt(n: &Mpi, d: &Mpi, c: &Mpi) -> Result<Mpi, MathError> {
    let (n, d, c) = (to_bn(n)?, to_bn(d)?, to_bn(c)?);
    let m = mod_exp(&c, &d, &n)?;
    Ok(Mpi::from_biguint(&m))
}

pub fn elgamal_encrypt(p: &Mpi, g: &Mpi, y: &Mpi, m: &Mpi) -> Result<(Mpi, Mpi), MathError> {
    let p = to_bn(p)?;
    let g = to_bn(g)?;
    let y = to_bn(y)?;
    let m = to_bn(m)?;

    let k_bits = decide_k_bits(p.bits() as usize);
    let k = rand_bits(k_bits)?;

    let c1 = mod_exp(&g, &k, &p)?;
    let yk = mod_exp(&y, &k, &p)?;
    let c2 = (&m * &yk) % &p;
    Ok((Mpi::from_biguint(&c1), Mpi::from_biguint(&c2)))
}

pub fn elgamal_decrypt(p: &Mpi, x: &Mpi, c1: &Mpi, c2: &Mpi) -> Result<Mpi, MathError> {
    let p = to_bn(p)?;
    let x = to_bn(x)?;
    let c1 = to_bn(c1)?;
    let c2 = to_bn(c2)?;

    let c1x = mod_exp(&c1, &x, &p)?;
    let inv = mod_inverse(&c1x, &p).ok_or_else(math_failed)?;
    let m = (&c2 * &inv) % &p;
    Ok(Mpi::from_biguint(&m))
}

fn mod_inverse(a: &BigUint, m: &BigUint) -> Option<BigUint> {
    use ::num_bigint::BigInt;
    use ::num_bigint::Sign;
    // OpenSSL int_bn_mod_inverse (bn_gcd.c) rejects a modulus of 0 or 1.
    if m.is_zero() || m.is_one() {
        return None;
    }
    let a = BigInt::from_biguint(Sign::Plus, a.clone());
    let m_i = BigInt::from_biguint(Sign::Plus, m.clone());

    let (mut old_r, mut r) = (a.clone(), m_i.clone());
    let (mut old_s, mut s) = (BigInt::one(), BigInt::zero());
    while !r.is_zero() {
        let q = &old_r / &r;
        let new_r = &old_r - &q * &r;
        old_r = std::mem::replace(&mut r, new_r);
        let new_s = &old_s - &q * &s;
        old_s = std::mem::replace(&mut s, new_s);
    }
    // gcd must be 1 for an inverse to exist.
    if old_r != BigInt::one() {
        return None;
    }
    let m_i2 = m_i.clone();
    let mut res = old_s % &m_i2;
    if res.sign() == Sign::Minus {
        res += &m_i2;
    }
    res.to_biguint()
}
