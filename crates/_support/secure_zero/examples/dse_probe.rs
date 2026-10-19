//! Codegen probe for scripts/secure-zero-dse-probe.sh: `wipe_dead_stack` wipes
//! a buffer that is dead afterwards, the case dead-store elimination removes.

#[inline(never)]
pub fn wipe_dead_stack(seed: u8) -> u8 {
    let mut secret = [seed; 64];
    let first = secret[0];
    secure_zero::secure_zero(&mut secret);
    first
}

fn main() {
    let seed = std::env::args().count() as u8;
    println!("{}", wipe_dead_stack(seed));
}
