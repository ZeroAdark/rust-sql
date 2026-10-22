// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <compare>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <string_view>
#include <utility>
#include <vector>
#include <unistd.h>
#include <sys/mman.h>
#include <disarm64.h>
#include <fadec-enc2.h>
#include "tpde/ElfMapper.hpp"
#include "tpde/AssemblerElf.hpp"
#include "tpde/ELF.hpp"
#include "tpde/base.hpp"
#include "tpde/util/SmallVector.hpp"
#include "tpde/util/misc.hpp"

#if defined(__aarch64__)
static constexpr bool NativeA64 = true;
#else
static constexpr bool NativeA64 = false;
#endif

#ifndef TPDE_ORACLE_X64
#define TPDE_ORACLE_X64 0
#endif

#pragma push_macro("__x86_64__")
#pragma push_macro("__aarch64__")
#undef __x86_64__
#undef __aarch64__
#if TPDE_ORACLE_X64
#define __x86_64__ 1
#else
#define __aarch64__ 1
#endif
#include "tpde/src/ElfMapper.cpp"
#pragma pop_macro("__aarch64__")
#pragma pop_macro("__x86_64__")

using namespace tpde;
using namespace tpde::elf;

static uint8_t byte() {
  const int value = std::cin.get();
  if (value == EOF) throw std::runtime_error("truncated oracle input");
  return uint8_t(value);
}
static uint64_t number(unsigned bytes = 8) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) value |= uint64_t(byte()) << (i * 8);
  return value;
}
static std::string bytes() {
  const auto size = number(4);
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
  if (size) std::cout.write(static_cast<const char *>(data), size);
}

template <typename T> static void track(std::vector<T> &values, T value) {
  if (std::find(values.begin(), values.end(), value) == values.end())
    values.push_back(value);
}

__attribute__((no_sanitize("function")))
static uint64_t invoke(void *address, uint64_t a, uint64_t b) {
  return reinterpret_cast<uint64_t (*)(uint64_t, uint64_t)>(address)(a, b);
}

struct SymbolPointer {
  using type = Elf64_Sym *(AssemblerElf::*)(SymRef);
  friend type original_symbol_pointer(SymbolPointer);
};

template <typename Tag, typename Tag::type Member>
struct PrivateSymbolAccess {
  friend typename Tag::type original_symbol_pointer(Tag) { return Member; }
};

// Explicit instantiation permits a fixture accessor without editing the reference.
template struct PrivateSymbolAccess<SymbolPointer, &AssemblerElf::sym_ptr>;

struct Resolution {
  std::string name;
  uint8_t kind;
  uint64_t value;
};

int main() {
  try {
    const auto arch = byte();
    if (arch != unsigned(TPDE_ORACLE_X64))
      throw std::runtime_error("oracle architecture mismatch");
    std::unique_ptr<AssemblerElf> a;
    if (arch == 0) a = std::make_unique<AssemblerElfA64>();
    else a = std::make_unique<AssemblerElfX64>();
    std::vector<SecRef> sections;
    std::vector<SymRef> symbols;
    std::vector<Resolution> resolutions;
    std::vector<std::pair<uint64_t, uint64_t>> arguments;
    SymRef native_function;
    while (std::cin.peek() != EOF) {
      switch (byte()) {
      case 0: {
        const auto kind = SectionKind(byte());
        track(sections, a->create_section(kind));
        break;
      }
      case 1: {
        const auto kind = SectionKind(byte());
        track(sections, a->get_default_section(kind));
        break;
      }
      case 2: {
        Assembler::TargetInfo::SectionFlags flags;
        flags.type = number(4);
        flags.flags = number(4);
        flags.name = number(4);
        flags.align = byte();
        flags.has_relocs = byte();
        flags.is_bss = byte();
        track(sections, a->create_section(flags));
        break;
      }
      case 3: {
        const SecRef section(number(4));
        const auto name = bytes();
        a->rename_section(section, name);
        break;
      }
      case 4: {
        const auto kind = byte();
        const auto binding = Assembler::SymBinding(byte());
        const auto name = bytes();
        SymRef symbol;
        switch (kind) {
        case 0: symbol = a->sym_add_undef(name, binding); break;
        case 1: symbol = a->sym_predef_func(name, binding); break;
        case 2: symbol = a->sym_predef_data(name, binding); break;
        case 3: symbol = a->sym_predef_tls(name, binding); break;
        default: throw std::runtime_error("symbol kind");
        }
        track(symbols, symbol);
        break;
      }
      case 5: {
        const SymRef symbol(number(4));
        const SecRef section(number(4));
        const auto position = number();
        const auto size = number();
        a->sym_def(symbol, section, position, size);
        break;
      }
      case 6: {
        const SecRef section(number(4));
        const SymRef symbol(number(4));
        const uint32_t alignment = number(4);
        const auto data = bytes();
        uint32_t offset;
        a->sym_def_predef_data(section, symbol,
            std::span<const u8>(reinterpret_cast<const u8 *>(data.data()), data.size()),
            alignment, &offset);
        break;
      }
      case 7: {
        const SecRef section(number(4));
        const SymRef symbol(number(4));
        const uint32_t size = number(4);
        const uint32_t alignment = number(4);
        uint32_t offset;
        a->sym_def_predef_zero(section, symbol, size, alignment, &offset);
        break;
      }
      case 8: {
        const SecRef section(number(4));
        const SymRef symbol(number(4));
        const uint32_t size = number(4);
        const uint32_t alignment = number(4);
        const auto fill = byte();
        const auto offset = a->sym_def_predef_data(section, symbol, uint64_t(size), alignment);
        std::memset(a->get_section(section).data.data() + offset, fill, size);
        break;
      }
      case 9: {
        const SecRef section(number(4));
        const auto name = bytes();
        const auto binding = Assembler::SymBinding(byte());
        const uint32_t alignment = number(4);
        const auto data = bytes();
        uint32_t offset;
        track(symbols, a->sym_def_data(section, name,
            std::span<const u8>(reinterpret_cast<const u8 *>(data.data()), data.size()),
            alignment, binding, &offset));
        break;
      }
      case 10: {
        const SecRef section(number(4));
        track(symbols, a->section_symbol(section));
        break;
      }
      case 11: {
        const SymRef symbol(number(4));
        a->sym_set_visibility(symbol, AssemblerElf::SymVisibility(byte()));
        break;
      }
      case 12: {
        const SymRef destination(number(4));
        const SymRef source(number(4));
        a->sym_copy(destination, source);
        break;
      }
      case 13: {
        const SymRef symbol(number(4));
        const bool comdat = byte();
        track(sections, a->create_group_section(symbol, comdat));
        break;
      }
      case 14: {
        const SecRef group(number(4));
        const SecRef section(number(4));
        a->add_to_group(group, section);
        break;
      }
      case 15: {
        const bool init = byte();
        const SecRef group(number(4));
        track(sections, a->create_structor_section(init, group));
        break;
      }
      case 16: {
        const SecRef section(number(4));
        const auto data = bytes();
        auto &destination = a->get_section(section).data;
        const auto offset = destination.size();
        destination.resize_uninitialized(offset + data.size());
        std::memcpy(destination.data() + offset, data.data(), data.size());
        break;
      }
      case 17: {
        const SecRef section(number(4));
        const auto value = number();
        a->get_section(section).write(value);
        break;
      }
      case 18: {
        const SecRef section(number(4));
        const SymRef symbol(number(4));
        const uint32_t kind = number(4);
        const uint32_t offset = number(4);
        const auto addend = int64_t(number());
        a->reloc_sec(section, symbol, kind, offset, addend);
        break;
      }
      case 19: {
        const bool absolute = byte();
        const SecRef section(number(4));
        const SymRef symbol(number(4));
        const uint32_t offset = number(4);
        const auto addend = int64_t(number());
        if (absolute) a->reloc_abs(section, symbol, offset, addend);
        else a->reloc_pc32(section, symbol, offset, addend);
        break;
      }
      case 20: {
        const SecRef section(number(4));
        const auto start = number();
        const uint32_t offset = number(4);
        a->get_section(section).adjust_relocation_offsets(start, offset);
        break;
      }
      case 24: {
        const uint32_t target = number(4);
        SecRef last;
        do {
          last = a->create_section(SectionKind::ReadOnly);
          sections.push_back(last);
        } while (last.id() < target);
        break;
      }
      case 25: track(sections, SecRef(number(4))); break;
      case 26: track(symbols, SymRef(number(4))); break;
      case 29: {
        const auto name = bytes();
        const auto kind = byte();
        const auto value = number();
        if (kind > 2) throw std::runtime_error("resolver kind");
        resolutions.push_back({name, kind, value});
        break;
      }
      case 36: {
        if (!NativeA64 || arch != 0 || native_function.valid())
          throw std::runtime_error("native fixture requires native AArch64");
        const SecRef section(number(4));
        native_function = SymRef(number(4));
        const auto count = number(4);
        if (count > 65536) throw std::runtime_error("too many native arguments");
        for (uint64_t i = 0; i < count; ++i) {
          const auto first = number();
          const auto second = number();
          arguments.emplace_back(first, second);
        }
        auto &text = a->get_section(section);
        const auto offset = text.size();
        text.write(de64_ADDx(DA_GP(0), DA_GP(0), DA_GP(1)));
        text.write(de64_RET(DA_GP(30)));
        a->sym_def(native_function, section, offset, 8);
        break;
      }
      case 37: {
        const SymRef symbol(number(4));
        const auto value = number();
        auto *entry = (a.get()->*original_symbol_pointer(SymbolPointer{}))(symbol);
        if (entry->st_shndx != SHN_UNDEF)
          throw std::runtime_error("absolute symbol already defined");
        entry->st_shndx = SHN_ABS;
        entry->st_value = value;
        break;
      }
      case 38: {
        const SecRef section(number(4));
        a->get_section(section).align = number(4);
        break;
      }
      case 30: {
        if (std::cin.peek() != EOF) throw std::runtime_error("data after map");
        ElfMapper mapper;
        std::vector<std::string> calls;
        uintptr_t observed_base = 0;
        auto resolver = [&](std::string_view name) -> void * {
          const auto base = reinterpret_cast<uintptr_t>(mapper.get_mapped_range().first);
          observed_base = base;
          calls.emplace_back(name);
          for (auto it = resolutions.rbegin(); it != resolutions.rend(); ++it) {
            if (it->name != name) continue;
            if (it->kind == 0) return reinterpret_cast<void *>(uintptr_t(it->value));
            if (it->kind == 1) return reinterpret_cast<void *>(base + uintptr_t(it->value));
            return nullptr;
          }
          return nullptr;
        };
        const bool probe = std::getenv("TPDE_MAPPER_PROBE") != nullptr;
        if (probe && ::setenv("TPDE_MAPPER_PROBE_ARMED", "1", 1) != 0)
          throw std::runtime_error("cannot arm mapper unwind probe");
        const bool success = mapper.map(*a, resolver);
        const auto [mapped, size] = mapper.get_mapped_range();
        if (mapped) observed_base = reinterpret_cast<uintptr_t>(mapped);
        put(::getpagesize());
        put(success);
        put(observed_base);
        put(size);
        put(sections.size());
        for (const auto ref : sections) {
          const auto &section = a->get_section(ref);
          put(ref.id());
          put(section.addr);
          put(section.size());
        }
        put(symbols.size());
        for (const auto ref : symbols) {
          put(ref.id());
          put(success ? reinterpret_cast<uintptr_t>(mapper.get_sym_addr(ref)) : 0);
        }
        put(calls.size());
        for (const auto &name : calls) put_bytes(name.data(), name.size());
        put_bytes(mapped, mapped ? size : 0);
        put(success ? arguments.size() : 0);
        if (success && native_function.valid()) {
          auto *start = static_cast<char *>(mapped);
          __builtin___clear_cache(start, start + size);
          void *entry = mapper.get_sym_addr(native_function);
          for (auto [first, second] : arguments) put(invoke(entry, first, second));
        }
        mapper.reset();
        put(mapper.get_mapped_range().first != nullptr);
        put(mapper.get_mapped_range().second);
        mapper.reset();
        put(mapper.get_mapped_range().first != nullptr);
        put(mapper.get_mapped_range().second);
        if (probe && ::unsetenv("TPDE_MAPPER_PROBE_ARMED") != 0)
          throw std::runtime_error("cannot disarm mapper unwind probe");
        return 0;
      }
      default: throw std::runtime_error("invalid oracle operation");
      }
    }
    throw std::runtime_error("missing map operation");
  } catch (const std::exception &error) {
    std::cerr << error.what() << '\n';
    return 1;
  }
}
