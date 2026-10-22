// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <cstring>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>
#include "tpde/AssemblerElf.hpp"
#include "tpde/arm64/FunctionWriterA64.hpp"

using namespace tpde;

static uint8_t byte() {
  int value = std::cin.get();
  if (value == EOF) throw std::runtime_error("truncated operation");
  return uint8_t(value);
}
static uint64_t number(unsigned bytes = 8) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= uint64_t(byte()) << (8 * i);
  return value;
}
static void put(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (8 * i)));
}
static void blob(const void *data, size_t size) {
  put(size);
  std::cout.write(static_cast<const char *>(data), size);
}
class Writer : public a64::FunctionWriterA64 {
  bool has_function = false;
public:
  void begin_func(u32 align, u32 expected) {
    a64::FunctionWriterA64::begin_func(align, expected);
    has_function = true;
  }
  void state() {
    put(offset()); put(allocated_size());
    put(has_function ? func_begin : 0); put(has_function ? label_skew : 0);
    put(get_sec_ref().id()); put(get_section().align);
  }
  void tables() {
    put(jump_tables.size());
    for (JumpTable *table : jump_tables) {
      put(table->off); put(table->size); put(table->idx.id());
      put(table->tmp.id()); put(table->misc);
      for (Label label : table->labels()) put(u32(label));
    }
  }
  void pad_to(u32 end) {
    if (end % 4 || end < offset()) throw std::runtime_error("invalid pad target");
    while (offset() < end) write_inst(0xd503201f);
  }
  void branch(u32 label, u32 kind, u32 inst) {
    ensure_space(4);
    label_ref(Label(label), offset(), LabelFixupKind(kind));
    write_inst_unchecked(inst);
  }
  u32 word(u32 off) { u32 value; std::memcpy(&value, begin_ptr() + off, 4); return value; }
  SecRef table_section() { return get_jump_table_section(); }
  void flush_for_export() {
    if (has_function) ensure_space(1);
    flush();
  }
};

static int diagnostic(const std::string &name) {
  tpde::elf::AssemblerElfA64 assembler;
  Writer writer;
  writer.begin_module(assembler);
  writer.switch_section(assembler.get_section(assembler.get_default_section(SectionKind::Text)));
  if (name == "--limit") {
    writer.begin_func(4, 128u * 1024 * 1024);
    while (writer.offset() < 128u * 1024 * 1024) {
      writer.ensure_space(16384);
      writer.cur_ptr() += 16384;
    }
    try { writer.ensure_space(4); }
    catch (const std::runtime_error &error) {
      std::cout << error.what() << '\n';
      return 0;
    }
    throw std::runtime_error("missing 128 MiB refusal");
  }
  if (name == "--table-growth") {
    writer.begin_func(4, 32768);
    Label label = writer.label_create();
    writer.branch(u32(label), u32(LabelFixupKind::AARCH64_TEST_BR), 0x36000000);
    writer.pad_to(16372);
    auto &table = writer.create_jump_table(1, Reg(1), Reg(2), false);
    table.labels()[0] = label;
    const u32 before = writer.word(16372);
    put(table.off); put(writer.offset()); put(before);
    writer.pad_to(65536);
    writer.label_place(label, writer.offset());
    writer.finish_func();
    put(writer.word(16372)); put(writer.word(16376)); put(writer.word(0));
    return 0;
  }
  if (name == "--capped-flush") {
    writer.begin_func(4, 65536);
    Label label = writer.label_create();
    writer.branch(u32(label), u32(LabelFixupKind::AARCH64_BR), 0x14000000);
    writer.pad_to(32768);
    writer.label_place(label, writer.offset());
    writer.finish_func();
    writer.flush();
    put(writer.offset()); put(writer.allocated_size());
    put(writer.get_section().data.size()); put(writer.word(0));
    return 0;
  }
  if (name == "--wide-backward") {
    writer.begin_func(4, 65536);
    writer.pad_to(16);
    Label label = writer.label_create();
    writer.label_place(label, writer.offset());
    writer.pad_to(300000);
    writer.ensure_space(24);
    auto &table = writer.create_jump_table(1, Reg(1), Reg(2), false);
    table.labels()[0] = label;
    writer.finish_func();
    auto &section = assembler.get_section(writer.table_section());
    u32 entry; std::memcpy(&entry, section.data.data(), 4);
    put(table.off); put(section.data.size()); put(entry);
    put(i64(table.off + 12) + i64(i32(entry)) * 4);
    return 0;
  }
  if (name.starts_with("--veneer-")) {
    const unsigned skew = name.find("s16") != std::string::npos ? 16 : 0;
    const unsigned gap = name.ends_with("g8") ? 8 : name.ends_with("g4") ? 4 : 0;
    writer.begin_func(4, 65536);
    writer.pad_to(skew);
    Label label = writer.label_create();
    const u32 first = writer.offset();
    writer.branch(u32(label), u32(LabelFixupKind::AARCH64_TEST_BR), 0x36000000);
    writer.pad_to(16384);
    writer.ensure_space(4);
    writer.pad_to(writer.offset() + gap);
    const u32 second = writer.offset();
    writer.branch(u32(label), u32(LabelFixupKind::AARCH64_TEST_BR), 0x36000001);
    writer.pad_to(65536);
    writer.label_place(label, writer.offset());
    writer.remove_prologue_bytes(0, skew);
    writer.finish_func();
    put(first); put(second); put(writer.offset());
    put(writer.word(first - skew)); put(writer.word(second - skew));
    return 0;
  }
  throw std::runtime_error("unknown diagnostic");
}

int main(int argc, char **argv) {
  unsigned step = 0, operation = 0;
  try {
    if (argc == 2) return diagnostic(argv[1]);
    tpde::elf::AssemblerElfA64 assembler;
    const std::array<SecRef, 2> sections{
      assembler.get_default_section(SectionKind::Text), assembler.create_section(SectionKind::Text)};
    Writer writer;
    writer.begin_module(assembler);
    writer.switch_section(assembler.get_section(sections[0]));
    while (std::cin.peek() != EOF) {
      operation = byte(); ++step;
      std::array<u64, 4> args; for (u64 &arg : args) arg = number();
      const auto [x, y, z, t] = args;
      u32 count = number(4);
      if (count > 4096) throw std::runtime_error("too many table labels");
      std::vector<Label> labels(count);
      for (Label &label : labels) label = Label(number(4));
      std::cout.put(char(operation));
      switch (operation) {
      case 0: writer.begin_func(x, y); break;
      case 1: writer.ensure_space(x); break;
      case 2: writer.write_inst(x); break;
      case 3: put(writer.try_write_inst(x)); break;
      case 4: writer.write_inst_unchecked(x); break;
      case 5: writer.pad_to(x); break;
      case 6: put(u32(writer.label_create())); break;
      case 7: writer.label_place(Label(x), y == UINT64_MAX ? writer.offset() : y); break;
      case 8: writer.branch(x, y, z); break;
      case 9: {
        auto &table = writer.create_jump_table(labels.size(), Reg(x), Reg(y), z);
        std::copy(labels.begin(), labels.end(), table.labels().begin());
        writer.tables(); break;
      }
      case 10: writer.remove_prologue_bytes(x, y); break;
      case 11: writer.eh_begin_fde(SymRef{}); break;
      case 12: writer.eh_advance(x); break;
      case 13: writer.eh_end_fde(); break;
      case 14: writer.finish_func(); writer.tables(); blob(writer.begin_ptr(), writer.offset()); break;
      case 15: writer.flush_for_export(); writer.end_module(); break;
      case 16: put(writer.label_offset(Label(x))); break;
      case 17: writer.align(x); break;
      case 18: writer.flush_for_export(); writer.switch_section(assembler.get_section(sections.at(x))); break;
      default: throw std::runtime_error("unknown opcode");
      }
      writer.state();
    }
    assembler.finalize();
    auto object = assembler.build_object_file();
    std::cout.put(char(255)); blob(object.data(), object.size());
  } catch (const std::exception &error) {
    std::cerr << "operation " << step << " (opcode " << operation << "): " << error.what() << '\n';
    return 1;
  }
}
