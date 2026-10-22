// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <bit>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <iterator>
#include <limits>
#include <span>
#include <string>
#include <string_view>
#include <tuple>
#include <vector>

#include "../reference/tpde/src/test/TestIRAdaptor.hpp"
#include "tpde/Analyzer.hpp"
#include "tpde/util/AddressSanitizer.hpp"

using tpde::u32;
using tpde::test::TestIR;
using Adaptor = tpde::test::TestIRAdaptor;
constexpr u32 invalid = ~0u;

std::string_view name(std::string_view text) {
  return text.empty() ? "-" : text;
}

template <class Range> void words(std::string_view tag, Range &&range) {
  std::cout << tag << ' ' << std::ranges::distance(range);
  for (auto value : range) {
    std::cout << ' ' << static_cast<u32>(value);
  }
  std::cout << '\n';
}

void records(const TestIR &ir) {
  std::cout << "module " << ir.functions.size() << ' ' << ir.blocks.size()
            << ' ' << ir.values.size() << ' ' << ir.value_operands.size()
            << '\n';
  for (u32 i = 0; i < ir.functions.size(); ++i) {
    const auto &f = ir.functions[i];
    std::cout << "function " << i << ' ' << name(f.name) << ' '
              << f.declaration << ' ' << f.local_only << ' ' << f.has_call
              << ' ' << f.arg_begin_idx << ' ' << f.arg_end_idx << ' '
              << f.block_begin_idx << ' ' << f.block_end_idx << '\n';
  }
  for (u32 i = 0; i < ir.blocks.size(); ++i) {
    const auto &b = ir.blocks[i];
    std::cout << "block " << i << ' ' << name(b.name) << ' '
              << b.succ_begin_idx << ' ' << b.succ_end_idx << ' '
              << b.inst_begin_idx << ' ' << b.phi_end_idx << ' '
              << b.inst_end_idx << ' ' << b.block_info << ' ' << b.block_info2
              << '\n';
  }
  for (u32 i = 0; i < ir.values.size(); ++i) {
    const auto &v = ir.values[i];
    const bool arg = v.type == TestIR::Value::Type::arg;
    std::cout << "value " << i << ' ' << name(v.name) << ' '
              << static_cast<u32>(v.type) << ' ' << static_cast<u32>(v.op)
              << ' ' << v.force_fixed_assignment << ' '
              << (v.op == TestIR::Value::Op::call ? v.call_func_idx : invalid)
              << ' ' << (arg ? 0 : v.op_count) << ' '
              << (arg ? 0 : v.op_begin_idx) << ' '
              << (arg ? 0 : v.op_end_idx) << '\n';
  }
  words("operands", ir.value_operands);
}

void adaptor_snapshot(TestIR &ir) {
  Adaptor a(&ir);
  a.start_compile();
  std::cout << "adaptor " << static_cast<u32>(a.INVALID_VALUE_REF) << ' '
            << static_cast<u32>(a.INVALID_BLOCK_REF) << ' '
            << static_cast<u32>(a.INVALID_FUNC_REF) << ' '
            << a.TPDE_PROVIDES_HIGHEST_VAL_IDX << ' '
            << a.TPDE_LIVENESS_VISIT_ARGS << ' ' << a.func_count() << '\n';
  words("funcs", a.funcs());
  words("compile", a.funcs_to_compile());
  for (auto f : a.funcs()) {
    const auto &function = ir.functions[static_cast<u32>(f)];
    std::cout << "function " << static_cast<u32>(f) << ' '
              << name(a.func_link_name(f)) << ' ' << a.func_extern(f) << ' '
              << a.func_only_local(f) << ' ' << a.func_has_weak_linkage(f)
              << '\n';
    if (function.declaration ||
        function.block_begin_idx == function.block_end_idx) {
      std::cout << "skip\n";
      continue;
    }
    const bool switched = a.switch_func(f);
    std::cout << "current " << switched << ' ' << a.cur_highest_val_idx()
              << ' ' << a.cur_needs_unwind_info() << ' ' << a.cur_is_vararg()
              << ' ' << a.cur_has_dynamic_alloca() << ' '
              << static_cast<u32>(a.cur_entry_block()) << '\n';
    words("args", a.cur_args());
    u32 arg_index = 0;
    for (auto v : a.cur_args()) {
      std::cout << "arg " << static_cast<u32>(v) << ' '
                << a.cur_arg_is_byval(arg_index) << ' '
                << a.cur_arg_byval_align(arg_index) << ' '
                << a.cur_arg_byval_size(arg_index) << ' '
                << a.cur_arg_is_sret(arg_index) << '\n';
      ++arg_index;
    }
    words("allocas", a.cur_static_allocas());
    const auto end = ir.blocks[function.block_end_idx - 1].inst_end_idx;
    for (u32 i = function.arg_begin_idx; i < end; ++i) {
      const auto v = Adaptor::IRValueRef(i);
      const bool alloca = ir.values[i].op == TestIR::Value::Op::alloca;
      std::cout << "value " << i << ' ' << name(a.value_fmt_ref(v)) << ' '
                << static_cast<u32>(a.val_local_idx(v)) << ' '
                << a.val_is_phi(v) << ' '
                << a.val_ignore_in_liveness_analysis(v) << ' '
                << (alloca ? a.val_alloca_size(v) : invalid) << ' '
                << (alloca ? a.val_alloca_align(v) : invalid) << '\n';
    }
    words("blocks", a.cur_blocks());
    for (auto b : a.cur_blocks()) {
      const u32 i = static_cast<u32>(b);
      const u32 first = a.block_info(b), second = a.block_info2(b);
      a.block_set_info(b, i ^ 0xa5a5a5a5u);
      a.block_set_info2(b, i ^ 0x5a5a5a5au);
      std::cout << "block " << i << ' ' << name(a.block_fmt_ref(b)) << ' '
                << first << ' ' << second << ' ' << a.block_info(b) << ' '
                << a.block_info2(b) << '\n';
      a.block_set_info(b, first);
      a.block_set_info2(b, second);
      words("succs", a.block_succs(b));
      words("phis", a.block_phis(b));
      words("insts", a.block_insts(b));
      for (auto v : a.block_phis(b)) {
        const auto phi = a.val_as_phi(v);
        std::cout << "phi " << static_cast<u32>(v) << ' '
                  << phi.incoming_count() << '\n';
        for (u32 slot = 0; slot < phi.incoming_count(); ++slot) {
          const auto block = phi.incoming_block_for_slot(slot);
          std::cout << "incoming " << slot << ' '
                    << static_cast<u32>(phi.incoming_val_for_slot(slot))
                    << ' ' << static_cast<u32>(block) << ' '
                    << static_cast<u32>(phi.incoming_val_for_block(block))
                    << '\n';
        }
        std::cout << "missing "
                  << static_cast<u32>(
                         phi.incoming_val_for_block(a.INVALID_BLOCK_REF))
                  << '\n';
      }
      for (auto inst : a.block_insts(b)) {
        std::cout << "inst " << static_cast<u32>(inst) << ' '
                  << name(a.inst_fmt_ref(inst)) << ' ' << a.inst_fused(inst)
                  << '\n';
        words("operands", a.inst_operands(inst));
        words("results", a.inst_results(inst));
      }
    }
    a.reset();
    std::cout << "reset " << a.cur_highest_val_idx() << '\n';
  }
  a.end_compile();
}

void analyze(TestIR &ir) {
  Adaptor a(&ir);
  tpde::Analyzer<Adaptor> analyzer(&a);
  using Index = decltype(analyzer)::BlockIndex;
  a.start_compile();
  for (auto f : a.funcs()) {
    const auto &function = ir.functions[static_cast<u32>(f)];
    std::cout << "function " << static_cast<u32>(f) << ' '
              << name(a.func_link_name(f)) << '\n';
    if (function.declaration ||
        function.block_begin_idx == function.block_end_idx) {
      std::cout << "skip\n";
      continue;
    }
    if (!a.switch_func(f)) {
      std::abort();
    }
    analyzer.switch_func(f);
    std::cout << "state " << analyzer.liveness_epoch << ' '
              << analyzer.liveness_max_value << ' ' << analyzer.num_insts
              << '\n';
    words("layout", analyzer.block_layout);
    words("map", analyzer.block_loop_map);
    std::cout << "loops " << analyzer.loops.size() << '\n';
    for (u32 i = 0; i < analyzer.loops.size(); ++i) {
      const auto &l = analyzer.loop_from_idx(i);
      std::cout << "loop " << i << ' ' << l.level << ' ' << l.parent << ' '
                << static_cast<u32>(l.begin) << ' ' << static_cast<u32>(l.end)
                << ' ' << l.num_blocks << ' ' << l.definitions << ' '
                << l.definitions_in_childs << '\n';
    }
    std::cout << "values " << analyzer.liveness_max_value + 1 << '\n';
    for (u32 i = 0; i <= analyzer.liveness_max_value; ++i) {
      const auto &l = analyzer.liveness[i];
      if (l.epoch == analyzer.liveness_epoch) {
        const auto &checked =
            analyzer.liveness_info(static_cast<tpde::ValLocalIdx>(i));
        std::cout << "live " << i << ' ' << checked.epoch << ' '
                  << static_cast<u32>(checked.first) << ' '
                  << static_cast<u32>(checked.last) << ' ' << checked.ref_count
                  << ' ' << checked.lowest_common_loop << ' '
                  << checked.last_full << '\n';
      } else {
        std::cout << "ignored " << i << ' ' << l.epoch << '\n';
      }
    }
    std::cout << "next "
              << static_cast<u32>(analyzer.block_ref(
                     static_cast<Index>(analyzer.block_layout.size())))
              << '\n';
    std::vector<std::pair<u32, u32>> before;
    for (auto b : a.cur_blocks()) {
      before.emplace_back(a.block_info(b), a.block_info2(b));
    }
    std::cout << "rpo\n";
    analyzer.print_rpo(std::cout);
    u32 i = 0;
    for (auto b : a.cur_blocks()) {
      if (before[i++] != std::make_pair(a.block_info(b), a.block_info2(b))) {
        std::abort();
      }
    }
    std::cout << "layout_print\n";
    analyzer.print_block_layout(std::cout);
    std::cout << "loops_print\n";
    analyzer.print_loops(std::cout);
    std::cout << "liveness_print\n";
    analyzer.print_liveness(std::cout);
    std::cout << "metadata " << function.block_end_idx - function.block_begin_idx
              << '\n';
    for (auto b : a.cur_blocks()) {
      std::cout << "block " << static_cast<u32>(b) << ' '
                << (a.block_info2(b) == 0 ? invalid : a.block_info(b)) << ' '
                << a.block_info2(b) << ' '
                << analyzer.block_has_multiple_incoming(b) << ' '
                << analyzer.block_has_phis(b) << '\n';
    }
    std::cout << "end\n";
  }
  a.end_compile();
}

int main() {
  std::string mode;
  if (!std::getline(std::cin, mode)) {
    return 64;
  }
  if (mode == "sanitizer") {
    std::cout << "sanitizer " << tpde::util::address_sanitizer_active << '\n';
    return 0;
  }
  if (mode != "parse" && mode != "print" && mode != "records" &&
      mode != "adaptor" && mode != "analyze") {
    std::cerr << "expected parse, print, records, adaptor, or analyze mode\n";
    return 64;
  }
  const std::string input{std::istreambuf_iterator<char>(std::cin),
                          std::istreambuf_iterator<char>()};
  const auto line = input.rfind('\n');
  const auto last_line = std::string_view(input).substr(
      line == std::string::npos ? 0 : line + 1);
  if (last_line.find(';') != std::string_view::npos) {
    std::cerr << "unterminated final comment is outside the upstream oracle domain\n";
    return 64;
  }
  if (std::any_of(input.begin(), input.end(), [](unsigned char c) {
        return c >= 128;
      })) {
    std::cerr << "non-ASCII input is outside the upstream oracle domain\n";
    return 64;
  }
  TestIR ir;
  const bool parsed = ir.parse_ir(input);
  std::cout << "parse " << parsed << '\n';
  if (!parsed || mode == "parse") {
    return 0;
  }
  if (mode == "print") {
    ir.print();
  } else if (mode == "records") {
    records(ir);
  } else if (mode == "adaptor") {
    adaptor_snapshot(ir);
  } else {
    analyze(ir);
  }
}
