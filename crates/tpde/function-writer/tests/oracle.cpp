// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <cstring>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <vector>
#include "tpde/AssemblerElf.hpp"
#include "tpde/FunctionWriter.hpp"
using namespace tpde;
using namespace tpde::elf;

static uint8_t byte() {
  int v = std::cin.get();
  if (v == EOF) throw std::runtime_error("truncated operation");
  return uint8_t(v);
}
static uint64_t number(unsigned bytes = 8) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= uint64_t(byte()) << (i * 8);
  return value;
}
static void put(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (i * 8)));
}
static void put_bytes(const void *data, size_t size) {
  put(size);
  std::cout.write(static_cast<const char *>(data), size);
}
class Writer : public FunctionWriter<Writer> {
  static constexpr std::array<u8, 5> instructions{0x0c, 7, 8, 0x90, 1};
  static constexpr TargetCIEInfo info{{instructions}, 16, 1, 0x78};
public:
  unsigned finishes = 0;
  Writer() : FunctionWriter(info) {}
  using FunctionWriterBase::alloc_jump_table;
  using FunctionWriterBase::get_jump_table_section;
  using FunctionWriterBase::more_space;
  void handle_fixups() { ++finishes; }
  void begin_base() { FunctionWriterBase::begin_func(); }
  void state() {
    put(offset()); put(allocated_size()); put(func_begin); put(label_skew);
    put(get_sec_ref().id()); put(get_section().align);
  }
  void fixups() {
    put(label_fixups.size());
    for (auto f : label_fixups) { put(u32(f.label)); put(f.off); put(u8(f.kind)); }
  }
  void table(const JumpTable &jt) {
    put(jt.size); put(jt.off); put(jt.idx.id()); put(jt.tmp.id()); put(jt.misc);
    for (auto l : const_cast<JumpTable &>(jt).labels()) put(u32(l));
  }
  void tables() {
    put(jump_tables.size());
    for (auto jt : jump_tables) table(*jt);
  }
};

template <class T> static void integral(Writer &w, u64 value, bool unchecked) {
  if (unchecked) w.write_unchecked<T>(T(value)); else w.write<T>(T(value));
}
int main() {
  unsigned step = 0, operation = 0;
  try {
    std::unique_ptr<AssemblerElf> a;
    if (byte() == 0) a = std::make_unique<AssemblerElfA64>();
    else a = std::make_unique<AssemblerElfX64>();
    std::array<SymRef, 5> syms{SymRef{},
      a->sym_add_undef("personality.0", Assembler::SymBinding::GLOBAL),
      a->sym_add_undef("personality.1", Assembler::SymBinding::GLOBAL),
      a->sym_add_undef("type.0", Assembler::SymBinding::GLOBAL),
      a->sym_add_undef("type.1", Assembler::SymBinding::GLOBAL)};
    std::array<SecRef, 2> secs{a->get_default_section(SectionKind::Text),
                             a->create_section(SectionKind::Text)};
    Writer w;
    w.begin_module(*a);
    while (std::cin.peek() != EOF) {
      operation = byte(); ++step;
      u64 args[4]; for (auto &arg : args) arg = number();
      u32 len = number(4);
      if (len > (1u << 20)) throw std::runtime_error("oversize payload");
      std::vector<u8> data(len);
      for (auto &v : data) v = byte();
      const auto [x,y,z,t] = std::array<u64,4>{args[0],args[1],args[2],args[3]};
      std::cout.put(char(operation));
      switch (operation) {
      case 0: w.switch_section(a->get_section(secs.at(x))); put(w.get_sec_ref().id()); break;
      case 1: w.begin_func(x,y); w.state(); break;
      case 2: case 3:
        switch (x) {
        case 0: integral<u8>(w,y,operation==3); break;
        case 1: integral<i8>(w,y,operation==3); break;
        case 2: integral<u16>(w,y,operation==3); break;
        case 3: integral<i16>(w,y,operation==3); break;
        case 4: integral<u32>(w,y,operation==3); break;
        case 5: integral<i32>(w,y,operation==3); break;
        case 6: integral<u64>(w,y,operation==3); break;
        case 7: integral<i64>(w,y,operation==3); break;
        case 8: integral<bool>(w,y,operation==3); break;
        default: throw std::runtime_error("integral kind");
        }
        put(w.offset()); break;
      case 4: w.ensure_space(x); w.state(); break;
      case 5: w.align(x); w.state(); break;
      case 6: put(u32(w.label_create())); break;
      case 7: put(w.label_is_pending(Label(x))); break;
      case 8: w.label_place(Label(x),y); break;
      case 9: put(w.label_offset(Label(x))); break;
      case 10: w.label_ref(Label(x),y,LabelFixupKind(z)); break;
      case 11: w.fixups(); break;
      case 12: w.remove_prologue_bytes(x,y); w.state(); break;
      case 13: w.reloc(syms.at(x),y,z,i64(t)); break;
      case 14: {
        if (data.size() != x * 4) throw std::runtime_error("jump label payload");
        auto &jt = w.alloc_jump_table(x,Reg(y),Reg(z)); jt.misc=t;
        for (u32 i=0;i<x;++i) { u32 l; std::memcpy(&l,data.data()+4*i,4); jt.labels()[i]=Label(l); }
        w.table(jt); break;
      }
      case 15: w.tables(); break;
      case 16: put(w.get_jump_table_section().id()); break;
      case 17: w.eh_begin_fde(syms.at(x)); break;
      case 18:
        if (x==0) w.eh_write_inst(y);
        else if(x==1) w.eh_write_inst(y,z);
        else if(x==2) w.eh_write_inst(y,z,t);
        else throw std::runtime_error("instruction arity");
        break;
      case 19: if(x) w.eh_advance(y); else w.eh_advance_raw(y); break;
      case 20: w.eh_align_frame(); break;
      case 21: w.eh_end_fde(); break;
      case 22: w.except_add_call_site(x,y,Label(z),t); break;
      case 23: w.except_add_cleanup_action(); break;
      case 24: w.except_add_action(x,syms.at(y)); break;
      case 25: w.except_add_empty_spec_action(x); break;
      case 26: put(w.except_type_idx_for_sym(syms.at(x))); break;
      case 27: w.except_encode_func(); break;
      case 28: w.finish_func(); put(w.finishes); break;
      case 29: w.flush(); put(w.offset()); put(w.get_section().data.size()); break;
      case 30: w.end_module(); break;
      case 31: put_bytes(w.begin_ptr(),w.offset()); break;
      case 32: {
        u8 result[21];
        u32 n=x==1 ? Writer::write_eh_inst(result,y,z) : Writer::write_eh_inst(result,y,z,t);
        put_bytes(result,n); break;
      }
      case 33: w.begin_base(); w.state(); break;
      case 34: {
        w.ensure_space(data.size());
        for (auto v : data) w.write_unchecked(v);
        put(w.offset()); break;
      }
      default: throw std::runtime_error("unknown operation");
      }
    }
    a->finalize();
    auto object=a->build_object_file();
    std::cout.put(char(255)); put_bytes(object.data(),object.size());
  } catch(const std::exception &e) {
    std::cerr << "operation " << step << " (opcode " << operation << "): " << e.what() << '\n';
    return 1;
  }
}
