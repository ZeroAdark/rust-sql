// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![no_std]

#![allow(non_snake_case)]

mod types;
pub use types::*;
pub mod runtime;

mod raw {
    include!(concat!(env!("OUT_DIR"), "/raw.rs"));
}

include!(concat!(env!("OUT_DIR"), "/wrappers.rs"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    InvalidOperand,
    InvalidEncoding,
    BufferTooSmall { required: usize, remaining: usize },
}

/// ```
/// use tpde_fadec::{Output, fe64_NOP};
/// let mut bytes = [0xa5; 64];
/// let mut output = Output::new(&mut bytes, 0x1000);
/// assert_eq!(fe64_NOP(&mut output, 40), Ok(40));
/// assert_eq!(output.address(), 0x1028);
/// ```
/// ```compile_fail
/// use tpde_fadec::{Output, fe64_NOP};
/// let mut bytes = [0; 8];
/// let mut output = Output::new(&mut bytes, 0);
/// bytes[0] = 1;
/// fe64_NOP(&mut output, 1).unwrap();
/// ```
/// ```compile_fail
/// use tpde_fadec::FeRegGP;
/// let invalid = FeRegGP { idx: 16 };
/// ```
pub struct Output<'a> {
    buffer: &'a mut [u8],
    address: usize,
    length: usize,
    scratch: [u8; 32],
}

impl<'a> Output<'a> {
    pub fn new(buffer: &'a mut [u8], address: usize) -> Self {
        Self {
            buffer,
            address,
            length: 0,
            scratch: [0; 32],
        }
    }

    pub const fn len(&self) -> usize {
        self.length
    }
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn remaining(&self) -> usize {
        self.buffer.len() - self.length
    }
    pub const fn address(&self) -> usize {
        self.address.wrapping_add(self.length)
    }
    pub fn bytes(&self) -> &[u8] {
        &self.buffer[..self.length]
    }

    fn encode(
        &mut self,
        encode: impl FnOnce(&mut [u8; 32], usize) -> usize,
    ) -> Result<usize, EncodeError> {
        let address = self.address();
        let length = encode(&mut self.scratch, address);
        if length == 0 || length > self.scratch.len() {
            return Err(EncodeError::InvalidEncoding);
        }
        self.admit(length)?;
        self.buffer[self.length..self.length + length].copy_from_slice(&self.scratch[..length]);
        self.length += length;
        Ok(length)
    }

    fn admit(&self, length: usize) -> Result<(), EncodeError> {
        if length > self.remaining() {
            return Err(EncodeError::BufferTooSmall {
                required: length,
                remaining: self.remaining(),
            });
        }
        Ok(())
    }
}

pub fn fe64_NOP(output: &mut Output<'_>, flags: u32) -> Result<usize, EncodeError> {
    const TABLE: [u8; 45] = [
        0x90, 0x66, 0x90, 0x0f, 0x1f, 0x00, 0x0f, 0x1f, 0x40, 0x00, 0x0f, 0x1f, 0x44, 0x00, 0x00,
        0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00, 0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00, 0x0f, 0x1f,
        0x84, 0x00, 0x00, 0x00, 0x00, 0x00, 0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let length = if flags == 0 { 1 } else { flags as usize };
    output.admit(length)?;
    let mut remainder = &mut output.buffer[output.length..output.length + length];
    while remainder.len() > 9 {
        remainder[..9].copy_from_slice(&TABLE[36..]);
        remainder = &mut remainder[9..];
    }
    let start = remainder.len() * (remainder.len() - 1) / 2;
    remainder.copy_from_slice(&TABLE[start..start + remainder.len()]);
    output.length += length;
    Ok(length)
}

#[cfg(feature = "upstream-oracle")]
pub mod oracle {
    pub use crate::raw::*;
    pub use crate::runtime::Runtime;
    // The build directory as a generated constant, not `env!("OUT_DIR")` as a value: a value use of
    // OUT_DIR anywhere in this file makes kache key the crate on the absolute target path (every fresh
    // target dir a 136 MiB miss, docs/gravity/reports-0919/build-cold.md); an `include!` is a path-only use.
    include!(concat!(env!("OUT_DIR"), "/build_directory.rs"));

    use crate::types::*;
    pub trait RawRegister {
        fn from_index(index: u8) -> Self;
    }
    macro_rules! raw_register {
        ($($ty:ident),* $(,)?) => {$(
            impl RawRegister for $ty {
                fn from_index(index: u8) -> Self { Self { idx: index } }
            }
        )*};
    }
    raw_register!(
        FeRegGP, FeRegGPH, FeRegGPLH, FeRegSREG, FeRegST, FeRegMM, FeRegXMM, FeRegMASK, FeRegTMM,
        FeRegCR, FeRegDR
    );

    pub fn raw_mem(flags: u8, base: u8, scale: u8, index: u8, off: i32) -> FeMem {
        FeMem {
            flags,
            base: FeRegGP { idx: base },
            scale,
            idx: FeRegGP { idx: index },
            off,
        }
    }

    pub fn raw_memv(flags: u8, base: u8, scale: u8, index: u8, off: i32) -> FeMemV {
        FeMemV {
            flags,
            base: FeRegGP { idx: base },
            scale,
            idx: FeRegXMM { idx: index },
            off,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nop_is_unbounded_and_atomic() {
        let mut bytes = [0xa5; 1031];
        let mut output = Output::new(&mut bytes[1..1030], 0x1000);
        assert_eq!(fe64_NOP(&mut output, 1024), Ok(1024));
        assert_eq!(output.address(), 0x1400);
        assert_eq!(output.len(), 1024);
        assert_eq!(
            fe64_NOP(&mut output, 6),
            Err(EncodeError::BufferTooSmall {
                required: 6,
                remaining: 5
            })
        );
        assert_eq!(output.len(), 1024);
        assert_eq!(fe64_NOP(&mut output, 0), Ok(1));
        assert_eq!(output.bytes()[1024], 0x90);
        assert_eq!(bytes[0], 0xa5);
        assert_eq!(&bytes[1026..], &[0xa5; 5]);
    }

    #[test]
    fn scratch_is_committed_only_on_success() {
        let mut bytes = [0xa5; 4];
        let mut output = Output::new(&mut bytes, usize::MAX);
        assert_eq!(
            output.encode(|scratch, address| {
                assert_eq!(address, usize::MAX);
                scratch.fill(0);
                0
            }),
            Err(EncodeError::InvalidEncoding)
        );
        assert!(output.is_empty());
        assert_eq!(
            output.encode(|scratch, _| {
                scratch.fill(3);
                5
            }),
            Err(EncodeError::BufferTooSmall {
                required: 5,
                remaining: 4
            })
        );
        assert!(output.is_empty());
        assert_eq!(
            output.encode(|scratch, _| {
                scratch[0] = 7;
                1
            }),
            Ok(1)
        );
        assert_eq!(output.address(), 0);
        assert_eq!(output.bytes(), &[7]);
        assert_eq!(bytes, [7, 0xa5, 0xa5, 0xa5]);
    }

    #[test]
    fn generated_encoders_reuse_dirty_scratch_after_failure() {
        let mut bytes = [0xa5; 32];
        let mut output = Output::new(&mut bytes, 0x1000);
        output.scratch.fill(0xff);
        assert_eq!(
            fe64_MOV8rr(&mut output, 0, FE_AH, FE_R8),
            Err(EncodeError::InvalidEncoding)
        );
        assert!(output.is_empty());
        assert_eq!(fe64_SSE_MOVAPSrr(&mut output, 0, FE_XMM0, FE_XMM1), Ok(3));
        assert_eq!(output.bytes(), &[0x0f, 0x28, 0xc1]);
        assert_eq!(
            fe64_MOV64rm(
                &mut output,
                0,
                FE_AX,
                FE_MEM(FE_IP, 0, FE_NOREG, 100).unwrap()
            ),
            Ok(7)
        );
        assert_eq!(&output.bytes()[3..], &[0x48, 0x8b, 5, 93, 0, 0, 0]);
        assert!(bytes[10..].iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn generated_relative_targets_use_committed_address() {
        let mut bytes = [0xa5; 16];
        let mut output = Output::new(&mut bytes, 0x1000);
        assert_eq!(fe64_NOP(&mut output, 3), Ok(3));
        assert_eq!(fe64_JMP(&mut output, 0, 0x1084), Ok(2));
        assert_eq!(&output.bytes()[3..], &[0xeb, 0x7f]);
        assert_eq!(fe64_JMP(&mut output, FE_JMPL, 0x1005), Ok(5));
        assert_eq!(&output.bytes()[5..], &[0xe9, 0xfb, 0xff, 0xff, 0xff]);
        assert_eq!(output.address(), 0x100a);
        assert!(bytes[10..].iter().all(|byte| *byte == 0xa5));
    }

    #[test]
    fn generated_capacity_failure_reuses_the_same_output() {
        let mut bytes = [0xa5; 6];
        let mut output = Output::new(&mut bytes[1..5], 0x1000);
        assert_eq!(
            fe64_JMP(&mut output, FE_JMPL, 0x1000),
            Err(EncodeError::BufferTooSmall {
                required: 5,
                remaining: 4
            })
        );
        assert_eq!(fe64_JMP(&mut output, 0, 0x1002), Ok(2));
        assert_eq!(output.bytes(), &[0xeb, 0]);
        assert_eq!(output.address(), 0x1002);
        assert_eq!(bytes, [0xa5, 0xeb, 0, 0xa5, 0xa5, 0xa5]);
    }
}

const _: () = {
    assert!(core::mem::size_of::<Output<'static>>() <= 128);
    assert!(!core::mem::needs_drop::<Output<'static>>());
};
