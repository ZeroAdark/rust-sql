
use super::consts::*;
use super::mpi::{self, read_mpi_pull, Mpi};
use super::pull::{expect_packet_end, getbyte, read_fixed, Dcx, PRes, Pull};
use super::pubkey::{KeyMaterial, PubKey, PGP_PUB_ELG_ENCRYPT, PGP_PUB_RSA_ENCRYPT,
    PGP_PUB_RSA_ENCRYPT_SIGN};

fn check_eme_pkcs1_v15(data: &[u8]) -> Option<usize> {
    if data.len() < 1 + 8 + 1 {
        return None;
    }
    if data[0] != 2 {
        return None;
    }
    let mut p = 1usize;
    let mut rnd = 0usize;
    while p < data.len() && data[p] != 0 {
        p += 1;
        rnd += 1;
    }
    if p == data.len() {
        return None;
    }
    if rnd < 8 {
        return None;
    }
    Some(p + 1)
}

fn control_cksum(msg: &[u8]) -> Result<(), &'static str> {
    if msg.len() < 3 {
        return Err(PUB_WRONG_KEY);
    }
    let mut my: u32 = 0;
    for &b in &msg[1..msg.len() - 2] {
        my += b as u32;
    }
    my &= 0xFFFF;
    let got = ((msg[msg.len() - 2] as u32) << 8) + msg[msg.len() - 1] as u32;
    if my != got {
        return Err("pubenc cksum failed");
    }
    Ok(())
}

fn padded_bytes(m: &Mpi) -> Vec<u8> {
    m.data.clone()
}

// px.c:70 PXE_PGP_WRONG_KEY, the pubenc session-key failures (pgp-pubdec.c).
const PUB_WRONG_KEY: &str = "Wrong key";

/// pgp-pubdec.c:148 pgp_parse_pubenc_sesskey.
pub fn parse_pubenc_sesskey(cx: &mut Dcx, pk: &PubKey, pkt: &mut dyn Pull) -> PRes<(i32, Vec<u8>)> {
    let ver = getbyte(pkt, cx)?;
    if ver != 3 {
        return Err(cx.corrupt(&format!("unknown pubenc_sesskey pkt ver={ver}")));
    }
    let key_id = read_fixed(pkt, cx, 8)?;
    if key_id != [0u8; 8] && key_id != pk.key_id {
        cx.dbg("key_id's does not match");
        return Err(PUB_WRONG_KEY.to_string());
    }
    let algo = getbyte(pkt, cx)? as i32;
    let m: Mpi = match algo {
        PGP_PUB_ELG_ENCRYPT => {
            let KeyMaterial::Elg { p, x, .. } = &pk.material else {
                return Err(PUB_WRONG_KEY.to_string());
            };
            if pk.algo != PGP_PUB_ELG_ENCRYPT {
                return Err(PUB_WRONG_KEY.to_string());
            }
            let x = x.as_ref().ok_or_else(|| WRONG_KEY.to_string())?;
            let c1 = read_mpi_pull(pkt, cx)?;
            let c2 = read_mpi_pull(pkt, cx)?;
            mpi::elgamal_decrypt(p, x, &c1, &c2).map_err(|e| e.report(cx.ctx))?
        }
        PGP_PUB_RSA_ENCRYPT | PGP_PUB_RSA_ENCRYPT_SIGN => {
            let KeyMaterial::Rsa { n, d, .. } = &pk.material else {
                return Err(PUB_WRONG_KEY.to_string());
            };
            if pk.algo != PGP_PUB_RSA_ENCRYPT && pk.algo != PGP_PUB_RSA_ENCRYPT_SIGN {
                return Err(PUB_WRONG_KEY.to_string());
            }
            let d = d.as_ref().ok_or_else(|| WRONG_KEY.to_string())?;
            let c = read_mpi_pull(pkt, cx)?;
            mpi::rsa_decrypt(n, d, &c).map_err(|e| e.report(cx.ctx))?
        }
        _ => return Err(UNKNOWN_PUBALGO.to_string()),
    };

    let padded = padded_bytes(&m);
    let Some(off) = check_eme_pkcs1_v15(&padded) else {
        cx.dbg("check_eme_pkcs1_v15 failed");
        return Err(PUB_WRONG_KEY.to_string());
    };
    let msg = &padded[off..];
    if let Err(why) = control_cksum(msg) {
        if why != PUB_WRONG_KEY {
            cx.dbg(why);
        }
        return Err(PUB_WRONG_KEY.to_string());
    }
    let sess_key_len = msg.len() - 3;
    if sess_key_len > PGP_MAX_KEY {
        cx.dbg(&format!("incorrect session key length={sess_key_len}"));
        return Err("Session key too big".to_string());
    }
    let cipher_algo = msg[0] as i32;
    let sess_key = msg[1..1 + sess_key_len].to_vec();
    expect_packet_end(pkt, cx)?;
    Ok((cipher_algo, sess_key))
}
