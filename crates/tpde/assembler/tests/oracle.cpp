// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>
#include "tpde/AssemblerElf.hpp"
using namespace tpde;
using namespace tpde::elf;

static uint8_t byte() {
  int v = std::cin.get();
  if (v == EOF) throw std::runtime_error("truncated oracle input");
  return uint8_t(v);
}
static uint64_t number(unsigned bytes = 8) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= uint64_t(byte()) << (i * 8);
  return value;
}
static std::string bytes() {
  auto size = number(4);
  if (size > (1u << 20)) throw std::runtime_error("oversize oracle input");
  std::string result(size, '\0');
  std::cin.read(result.data(), result.size());
  if (!std::cin) throw std::runtime_error("truncated oracle bytes");
  return result;
}
static void put(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (i * 8)));
}
static void put_bytes(const void *data, size_t size) {
  put(size);
  std::cout.write(static_cast<const char *>(data), size);
}
int main() {
  try {
    std::unique_ptr<AssemblerElf> a;
    if (byte() == 0) a = std::make_unique<AssemblerElfA64>();
    else a = std::make_unique<AssemblerElfX64>();
    while (std::cin.peek() != EOF) {
      auto op = byte();
      std::cout.put(char(op));
      switch (op) {
      case 0: { auto k = SectionKind(byte()); put(a->create_section(k).id()); break; }
      case 1: { auto k = SectionKind(byte()); put(a->get_default_section(k).id()); break; }
      case 2: {
        Assembler::TargetInfo::SectionFlags f;
        f.type=number(4); f.flags=number(4); f.name=number(4); f.align=byte();
        f.has_relocs=byte(); f.is_bss=byte();
        put(a->create_section(f).id()); break;
      }
      case 3: { SecRef s(number(4)); auto name=bytes(); a->rename_section(s,name); break; }
      case 4: {
        auto kind=byte(); auto binding=Assembler::SymBinding(byte()); auto name=bytes(); SymRef s;
        switch(kind) {
          case 0: s=a->sym_add_undef(name,binding); break;
          case 1: s=a->sym_predef_func(name,binding); break;
          case 2: s=a->sym_predef_data(name,binding); break;
          case 3: s=a->sym_predef_tls(name,binding); break;
          default: throw std::runtime_error("symbol kind");
        }
        put(s.id()); break;
      }
      case 5: { SymRef s(number(4)); SecRef sec(number(4)); auto pos=number(); auto size=number(); a->sym_def(s,sec,pos,size); break; }
      case 6: {
        SecRef sec(number(4)); SymRef s(number(4)); u32 align=number(4); auto data=bytes(); u32 off;
        a->sym_def_predef_data(sec,s,std::span<const u8>((const u8*)data.data(),data.size()),align,&off); put(off); break;
      }
      case 7: { SecRef sec(number(4)); SymRef s(number(4)); u32 size=number(4), align=number(4), off;
        a->sym_def_predef_zero(sec,s,size,align,&off); put(off); break; }
      case 8: {
        SecRef sec(number(4)); SymRef s(number(4)); u32 size=number(4), align=number(4); auto fill=byte();
        auto off=a->sym_def_predef_data(sec,s,u64(size),align);
        std::memset(a->get_section(sec).data.data()+off,fill,size); put(off); break;
      }
      case 9: {
        SecRef sec(number(4)); auto name=bytes(); auto binding=Assembler::SymBinding(byte());
        u32 align=number(4), off; auto data=bytes();
        auto s=a->sym_def_data(sec,name,std::span<const u8>((const u8*)data.data(),data.size()),align,binding,&off);
        put(s.id()); put(off); break;
      }
      case 10: { SecRef sec(number(4)); put(a->section_symbol(sec).id()); break; }
      case 11: { SymRef s(number(4)); auto v=AssemblerElf::SymVisibility(byte()); a->sym_set_visibility(s,v); break; }
      case 12: { SymRef dst(number(4)), src(number(4)); a->sym_copy(dst,src); break; }
      case 13: { SymRef s(number(4)); bool comdat=byte(); put(a->create_group_section(s,comdat).id()); break; }
      case 14: { SecRef group(number(4)), sec(number(4)); a->add_to_group(group,sec); break; }
      case 15: { bool init=byte(); SecRef group(number(4)); put(a->create_structor_section(init,group).id()); break; }
      case 16: {
        SecRef sec(number(4)); auto data=bytes(); auto &v=a->get_section(sec).data;
        auto pos=v.size(); v.resize_uninitialized(pos+data.size()); std::memcpy(v.data()+pos,data.data(),data.size()); break;
      }
      case 17: { SecRef sec(number(4)); u64 value=number(); a->get_section(sec).write(value); break; }
      case 18: { SecRef sec(number(4)); SymRef s(number(4)); u32 kind=number(4), offset=number(4); i64 addend=i64(number()); a->reloc_sec(sec,s,kind,offset,addend); break; }
      case 19: { bool abs=byte(); SecRef sec(number(4)); SymRef s(number(4)); u32 offset=number(4); i64 addend=i64(number());
        if(abs) a->reloc_abs(sec,s,offset,addend); else a->reloc_pc32(sec,s,offset,addend); break; }
      case 20: { SecRef sec(number(4)); auto start=number(); u32 offset=number(4); a->get_section(sec).adjust_relocation_offsets(start,offset); break; }
      case 22: { a->finalize(); auto out=a->build_object_file(); put_bytes(out.data(),out.size()); break; }
      case 23: { a->reset(); break; }
      case 24: { u32 target=number(4); SecRef last; do {last=a->create_section(SectionKind::ReadOnly);} while(last.id()<target); put(last.id()); break; }
      case 25: { SecRef sec(number(4)); bool reloc=byte(), name=byte(); auto &s=a->get_section(sec);
        put(s.get_ref().id()); put(s.size()); if(reloc) put(s.reloc_count());
        if(name) { auto n=a->sec_name(sec); put_bytes(n,std::strlen(n)); } break; }
      case 26: { SymRef s(number(4)); bool defined=byte(); auto n=a->sym_name(s); put_bytes(n,std::strlen(n)); if(defined) put(a->sym_section(s).id()); break; }
      default: throw std::runtime_error("invalid oracle operation");
      }
    }
  } catch(const std::exception &e) { std::cerr << e.what() << '\n'; return 1; }
}
