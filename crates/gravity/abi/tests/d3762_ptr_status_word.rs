//! D3762: a host's pointer answer that carries a status (`ERROR as *mut u8`) reaches the IR as the status, on a 32-bit
//! target too: there the pointer holds the status's low 32 bits, and zero-extended they read as a live pointer (a sort
//! whose comparison raised went on with an empty run on postgres.wasm, the error dropped).
use gravity_abi::host::words::{ptr32_word, ptr_word, IntoWord, STATUS_WINDOW};
use gravity_abi::status::{raise, raised, BUILD_WAIT, CANCELLED, ERROR, STOP, UNSUPPORTED, UNSUPPORTED_SPILL};

/// Every status a pointer answer carries, as the 32-bit pointer holds it.
fn statuses() -> Vec<i64> {
    let mut s = vec![STOP, CANCELLED, ERROR, BUILD_WAIT, -(-UNSUPPORTED | ((UNSUPPORTED_SPILL as i64) << 8))];
    s.extend((0..raise::CODES).map(|c| raised(raise::bit(c))));
    s.push(raised((1 << raise::CODES) - 1));
    s
}

#[test]
fn a_32_bit_pointer_status_widens_to_the_status() {
    for st in statuses() {
        assert!(st > -(STATUS_WINDOW as i64), "status {st} past the window");
        assert_eq!(ptr32_word(st as u32) as i64, st, "status {st}");
    }
}

#[test]
fn a_32_bit_live_pointer_widens_unchanged() {
    for p in [0u32, 1, 0x10, 0x7fff_ffff, 0x8000_0000, 0xc000_0000, u32::MAX - STATUS_WINDOW] {
        assert_eq!(ptr32_word(p), p as u64, "pointer {p:#x}");
    }
}

#[test]
fn a_pointer_answer_on_this_target_is_its_status() {
    for st in statuses() {
        assert_eq!((st as isize as *mut u8).into_word() as i64, st, "*mut status {st}");
        assert_eq!((st as isize as *const u32).into_word() as i64, st, "*const status {st}");
        assert_eq!(ptr_word(st as isize as usize) as i64, st);
    }
    let x = 7u64;
    assert_eq!((&x as *const u64).into_word(), &x as *const u64 as usize as u64);
}

/// The server binary's wasm32 memory stops below the window, so no live pointer is in it.
#[test]
fn the_wasm_memory_stops_below_the_window() {
    let build = include_str!("../../../backend/main/main_main/build.rs");
    let cap = (1u64 << 32) - STATUS_WINDOW as u64;
    assert_eq!(cap % 65536, 0, "a whole number of wasm pages");
    assert!(build.contains(&format!("cargo:rustc-link-arg-bins=--max-memory={cap}")), "main_main's build.rs caps the memory at {cap}");
}
