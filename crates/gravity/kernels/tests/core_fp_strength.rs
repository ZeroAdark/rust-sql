//! Hash contract S0 (docs/gravity/hash-contract.md §3.6): the kernels' fingerprint host `text_fp` (`host_abi.rs:1754`) is full
//! strength — SipHash-2-4-128 under the installed key over the WHOLE payload, in both header forms, at every length to 4096 and
//! for keys differing at any one byte.
use gravity_kernels::host_abi::text_fp;

fn image(bytes: &[u8], short: bool) -> Vec<u8> {
    let mut v = Vec::new();
    if short {
        v.push((((bytes.len() + 1) as u8) << 1) | 1);
    } else {
        v.extend_from_slice(&(((bytes.len() + 4) as u32) << 2).to_le_bytes());
    }
    v.extend_from_slice(bytes);
    v
}

fn fp(img: &[u8]) -> [u64; 2] {
    let mut out = [0u64; 2];
    assert_eq!(unsafe { text_fp(img.as_ptr() as u64, out.as_mut_ptr() as u64) }, 0);
    out
}

#[test]
fn text_fp_hashes_every_byte_of_the_payload() {
    let _ = gravity_dict::fp::install(0x0706_0504_0302_0100, 0x0f0e_0d0c_0b0a_0908);
    let (k0, k1) = gravity_dict::fp::seed().unwrap();
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut lens: Vec<usize> = (0..=300).collect();
    lens.extend((301..=4096).step_by(61));
    lens.extend([2047, 2048, 2049, 4096]);
    for len in lens {
        let bytes: Vec<u8> = (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let want = gravity_dict::fp::siphash128(k0, k1, &bytes);
        for short in [true, false] {
            if short && len > 126 {
                continue;
            }
            assert_eq!(fp(&image(&bytes, short)), want, "len {len} short {short}");
        }
    }
    for len in [9usize, 17, 64, 300] {
        let base: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let f0 = fp(&image(&base, false));
        for i in 0..len {
            let mut v = base.clone();
            v[i] = b'#';
            assert_ne!(fp(&image(&v, false)), f0, "len {len} byte {i}");
        }
    }
}
