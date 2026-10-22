// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <cstring>
#include <cstdlib>
#include <iostream>
#include <string>
#include <vector>
#include "tpde/AssemblerElf.hpp"
#include "tpde/x64/FunctionWriterX64.hpp"

using namespace tpde;
using namespace tpde::elf;

[[noreturn]] static void fail(const char *message) {
  std::cerr << message << '\n';
  std::exit(2);
}
static u8 byte() {
  int value = std::cin.get();
  if (value == EOF) fail("truncated input");
  return u8(value);
}
static u64 number(unsigned bytes = 8) {
  u64 value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= u64(byte()) << (i * 8);
  return value;
}
static void put(u64 value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (i * 8)));
}
static void blob(const u8 *data, size_t size) {
  put(size);
  if (size) std::cout.write(reinterpret_cast<const char *>(data), size);
}
class Writer : public x64::FunctionWriterX64 {
public:
  void state() const {
    put(offset()); put(func_begin); put(label_skew); put(get_sec_ref().id());
    put(get_section().align); put(label_offsets.size());
    put(label_fixups.size()); put(jump_tables.size());
    for (u32 i = 0; i < label_offsets.size(); ++i)
      put(label_is_pending(Label(i)) ? ~0u : label_offset(Label(i)));
  }
};

int main() {
  unsigned step = 0, operation = 0;
  AssemblerElfX64 assembler;
  std::array<SymRef, 3> syms{
    SymRef{},
    assembler.sym_add_undef("external", Assembler::SymBinding::GLOBAL),
    assembler.sym_add_undef("personality", Assembler::SymBinding::GLOBAL)};
  std::array<SecRef, 2> sections{
    assembler.get_default_section(SectionKind::Text),
    assembler.create_section(SectionKind::Text)};
  Writer writer;
  writer.begin_module(assembler);
  writer.switch_section(assembler.get_section(sections[0]));
  while (std::cin.peek() != EOF) {
    operation = byte();
    ++step;
    std::array<u64, 4> args;
    for (auto &arg : args) arg = number();
    const auto [a, b, c, d] = args;
    u32 len = number(4);
    if (len > (1u << 20)) fail("oversize payload");
    std::vector<u8> data(len);
    for (auto &value : data) value = byte();
    std::cout.put(char(operation));
    switch (operation) {
    case 1: writer.begin_func(a, b); break;
    case 2:
      writer.ensure_space(data.size());
      for (u8 value : data) writer.write_unchecked(value);
      break;
    case 3: writer.align(a); break;
    case 4: put(u32(writer.label_create())); break;
    case 5: writer.label_place(Label(a), b); break;
    case 6: writer.label_ref(Label(a), b, LabelFixupKind::X64_JMP_OR_MEM_DISP); break;
    case 7: writer.finish_func(); break;
    case 8: {
      if (data.size() != a * 4) fail("invalid table payload");
      auto &table = writer.create_jump_table(a, Reg(b), Reg(c));
      for (u32 i = 0; i < a; ++i) {
        u32 label;
        std::memcpy(&label, data.data() + i * 4, 4);
        table.labels()[i] = Label(label);
      }
      put(table.size); put(table.off); put(table.idx.id()); put(table.tmp.id());
      for (Label label : table.labels()) put(u32(label));
      break;
    }
    case 9: writer.remove_prologue_bytes(a, b); break;
    case 10: writer.eh_begin_fde(syms.at(a)); break;
    case 11: writer.eh_end_fde(); break;
    case 12: writer.eh_advance(a); break;
    case 13: writer.eh_write_inst(a, b, c); break;
    case 14: writer.reloc(syms.at(a), b, c, i64(d)); break;
    case 15: writer.flush(); break;
    case 16:
      writer.switch_section(assembler.get_section(sections.at(a)));
      break;
    case 17: {
      auto name = std::string(reinterpret_cast<const char *>(data.data()), data.size());
      SymRef symbol = assembler.sym_predef_func(name, Assembler::SymBinding::GLOBAL);
      assembler.sym_def(symbol, writer.get_sec_ref(), a, b);
      put(symbol.id());
      break;
    }
    default: fail("unknown operation");
    }
    writer.state();
  }
  writer.flush();
  writer.end_module();
  assembler.finalize();
  auto object = assembler.build_object_file();
  std::cout.put(char(255));
  blob(object.data(), object.size());
}
