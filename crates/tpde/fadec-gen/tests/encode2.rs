// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

pub const SHAPE_RUNTIME: &str = r#"
mod runtime {
    macro_rules! register {
        ($($name:ident),* $(,)?) => {$ (
            #[derive(Clone, Copy)]
            pub struct $name { pub idx: u8 }
        )*};
    }
    register!(FeRegGP, FeRegGPH, FeRegGPLH, FeRegSREG, FeRegST, FeRegMM,
              FeRegXMM, FeRegMASK, FeRegTMM, FeRegCR, FeRegDR);
    impl From<FeRegGP> for FeRegGPLH {
        fn from(reg: FeRegGP) -> Self { Self { idx: reg.idx } }
    }
    impl From<FeRegGPH> for FeRegGPLH {
        fn from(reg: FeRegGPH) -> Self { Self { idx: reg.idx | 0x20 } }
    }
    #[derive(Clone, Copy)]
    pub struct FeMem {
        pub flags: u8,
        pub base: FeRegGP,
        pub scale: u8,
        pub idx: FeRegGP,
        pub off: i32,
    }
    #[derive(Clone, Copy)]
    pub struct FeMemV {
        pub flags: u8,
        pub base: FeRegGP,
        pub scale: u8,
        pub idx: FeRegXMM,
        pub off: i32,
    }
    pub const FE_JMPL: u32 = 0x8;
    pub const FE_ADDR32: u32 = 0x10;
    pub const FE_SEG_MASK: u32 = 0x7;
    pub const FE_CC_MASK: u32 = 0xf0000;
    pub const FE_RC_MASK: u32 = 0x60;
}
"#;

#[test]
fn uniqueness_rejects_empty_and_conflicting_sets() {
    use tpde_fadec_gen::encode2::unique;
    assert_eq!(unique([false, false, false]).unwrap(), false);
    assert_eq!(
        unique([("GP", "XMM"), ("GP", "XMM")]).unwrap(),
        ("GP", "XMM")
    );
    assert!(unique([false, true]).is_err());
    assert!(unique(Vec::<bool>::new()).is_err());
}
