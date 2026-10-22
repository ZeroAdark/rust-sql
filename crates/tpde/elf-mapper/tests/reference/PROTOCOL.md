# ElfMapper pinned C++ oracle protocol

All integer input/output is little endian. `bytes` input is u32 length plus
payload; output `bytes` is u64 length plus payload. Input starts with one byte:
0=AArch64, 1=X86-64 (must match the compiled oracle target). The ordinary
Assembler construction operations 0–20 use exactly the existing
`crates/tpde/assembler/tests/oracle.cpp` input encoding, with no output.
References returned by creation are inferred from the matching Rust assembler.
Every created/default-touched section and every created symbol is tracked once
in first-touch order. No trailing data is permitted after operation 30.

Additional operations:

- 24: u32 desired section id; create ReadOnly sections until id>=desired.
- 25: u32 section id; track this section only.
- 26: u32 symbol id; track this symbol only.
- 29: input bytes name, u8 resolver kind, u64 value. Kind 0 returns value as
  address, 1 returns current mapper base + wrapping value, 2 returns null.
  Last configuration for a name wins. Missing names return null. Every resolver
  call is retained in order, including repeated failed weak resolution.
- 36: u32 text section id, u32 function symbol id, u32 argument-pair count,
  then pairs of u64 a, u64 b. Append the actual upstream Disarm encodings of
  ADD x0,x0,x1 and RET x30, and define the supplied predeclared symbol at the
  appended 8-byte body. The oracle invokes this function for every pair after
  mapping. Only permitted for the native AArch64 oracle. There is one such
  operation per script at most.
- 37: u32 symbol id, u64 value. Define an absolute ELF symbol by setting the
  original private Elf64_Sym storage's st_shndx=SHN_ABS and st_value=value.
  The symbol must previously be undefined. A test-only explicit-instantiation
  accessor obtains the original sym_ptr member without modifying pinned files.
- 38: u32 section id, u32 alignment. Assign the original section's public align
  field, permitting page-sized alignment cases beyond SectionFlags' u8 field.
- 30: Map using configured resolver and emit the result below, then reset twice
  and terminate. The fixture must create/touch its EHFrame section beforehand.

Operation 30 output, all scalar fields u64:

1. page_size, success(0/1), base_address, mapped_size. Base preserves the last
   resolver-observed mapping base on failure (0 if no resolver call occurred).
   Relocation/protection failure resets mapped_size to zero. A failed mmap
   preserves the upstream nonzero requested mapped_size with a null address;
   the image byte string is empty whenever the address is null.
2. section_count; for each tracked section: id, assigned_address_offset, size.
   Section size excludes the mapper-added 4-byte EH terminator.
3. symbol_count; for each tracked symbol: id, resolved_address (0 on failure).
4. resolver_call_count; for each call: output bytes name.
5. output bytes entire mapped image, including PLT/GOT, alignment gaps, and EH
   terminator. Empty on mapping failure. Absolute addresses are compared by
   relocating the Rust portable image at the C++ reported base address.
6. native_result_count then one u64 result per argument pair. Before invoking,
   the test adapter calls __builtin___clear_cache on the mapped image. This
   compensates for the upstream mapper's missing AArch64 cache-flush operation;
   it is not an alteration to the pinned mapper implementation. Only the call
   adapter disables UBSan's function-type check, because dynamically emitted
   native functions have no Clang function-type metadata before their entry;
   memory and other undefined-behavior checks remain enabled.
7. After first reset: mapped(0/1), mapped_size. After second reset: same pair.

The X86-64 oracle on AArch64 executes the exact pinned mapper source through a
wrapper that preincludes every dependency under native architecture macros,
then selects only the mapper's compile-time architecture branch. It does not
execute X86-64 instructions. Both upstream source files remain byte-identical.

Primary encoder references remain unchanged-source C11 at O3. Only the
supplemental sanitized Fadec object is compiled as C++20 at O1 with debug
information and address, undefined-behavior, and bounds sanitizers. Its
promoted no-index sentinel shift enters signed int's sign bit, which is
undefined in C and defined in C++20. This language-mode distinction does not
establish defined C semantics. Disarm remains C11 at O3 in both builds; the
mapper translation unit remains C++20 at O3. Assertion/NDEBUG variants and
all 324 cases / 1296 comparisons remain required. Compiler command receipts
record this policy, and source/command hashes invalidate previous caches.

Each differential attempt owns a fresh Rust context and fixture. Panics or
failed reference processes produce a FAIL roster row and a dedicated diagnostic
file, then the runner continues with independent comparisons. Process failure
is checked before output is interpreted, and a nonzero observed mapping base
is always required. Thus infrastructure failures cannot pass as relocation
refusals. The five unwind probes, native lifecycle, and OS/accounting checks
also run independently. `failures.tsv` and `execution-summary.tsv` distinguish
attempted and completed comparisons; all completion floors and an empty failure
collection are required before qualification PASS markers are emitted.
