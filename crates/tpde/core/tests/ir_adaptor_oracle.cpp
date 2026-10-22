// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <tpde/IRAdaptor.hpp>

#include <array>
#include <cstdio>
#include <span>
#include <string_view>

using tpde::u32;
constexpr u32 invalid = ~u32{0};

struct Phi {
  u32 incoming_count() const { return 2; }
  u32 incoming_val_for_slot(u32 slot) const { return slot == 0 ? 10 : 12; }
  u32 incoming_block_for_slot(u32 slot) const { return slot + 1; }
  u32 incoming_val_for_block(u32 block) const {
    return block == 1 ? 10 : block == 2 ? 12 : invalid;
  }
};

struct AdapterNoHighest {
  using IRValueRef = u32;
  using IRInstRef = u32;
  using IRBlockRef = u32;
  using IRFuncRef = u32;
  static constexpr u32 INVALID_VALUE_REF = invalid;
  static constexpr u32 INVALID_BLOCK_REF = invalid;
  static constexpr u32 INVALID_FUNC_REF = invalid;
  static constexpr bool TPDE_PROVIDES_HIGHEST_VAL_IDX = false;
  static constexpr bool TPDE_LIVENESS_VISIT_ARGS = true;
  static constexpr std::array<u32, 2> function_ids{0, 1};
  static constexpr std::array<u32, 1> compiled_ids{0};
  static constexpr std::array<u32, 2> arguments{10, 11};
  static constexpr std::array<u32, 1> allocations{15};
  static constexpr std::array<u32, 2> blocks{1, 2};
  static constexpr std::array<u32, 1> successor{2};
  static constexpr std::array<u32, 1> first_insts{20};
  static constexpr std::array<u32, 1> second_insts{21};
  static constexpr std::array<u32, 1> phis{13};
  static constexpr std::array<u32, 2> first_operands{10, 11};
  static constexpr std::array<u32, 1> second_operands{13};
  static constexpr std::array<u32, 1> first_results{12};
  static constexpr std::array<u32, 1> second_results{14};
  u32 current = invalid;
  std::array<u32, 3> info{};
  std::array<u32, 3> info2{};
  u32 starts = 0, ends = 0, switches = 0, resets = 0;

  u32 func_count() const { return 2; }
  std::span<const u32> funcs() const { return function_ids; }
  std::span<const u32> funcs_to_compile() const { return compiled_ids; }
  std::string_view func_link_name(u32 f) const {
    return f == 0 ? "main" : "external";
  }
  bool func_extern(u32 f) const { return f == 1; }
  bool func_only_local(u32 f) const { return f == 0; }
  bool func_has_weak_linkage(u32 f) const { return f == 1; }
  bool cur_needs_unwind_info() const { return true; }
  bool cur_is_vararg() const { return false; }
  std::span<const u32> cur_args() const { return arguments; }
  bool cur_arg_is_byval(u32 arg) const { return arg == 0; }
  u32 cur_arg_byval_size(u32 arg) const { return arg == 0 ? 24 : 0; }
  u32 cur_arg_byval_align(u32 arg) const { return arg == 0 ? 8 : 0; }
  bool cur_arg_is_sret(u32 arg) const { return arg == 1; }
  std::span<const u32> cur_static_allocas() const { return allocations; }
  bool cur_has_dynamic_alloca() const { return true; }
  u32 cur_entry_block() const { return 1; }
  std::span<const u32> cur_blocks() const { return blocks; }
  std::span<const u32> block_succs(u32 block) const {
    return block == 1 ? std::span<const u32>(successor) : std::span<const u32>{};
  }
  std::span<const u32> block_insts(u32 block) const {
    return block == 1 ? first_insts : second_insts;
  }
  std::span<const u32> block_phis(u32 block) const {
    return block == 2 ? std::span<const u32>(phis) : std::span<const u32>{};
  }
  u32 block_info(u32 block) const { return info[block]; }
  void block_set_info(u32 block, u32 value) { info[block] = value; }
  u32 block_info2(u32 block) const { return info2[block]; }
  void block_set_info2(u32 block, u32 value) { info2[block] = value; }
  std::string_view block_fmt_ref(u32 block) const {
    return block == 1 ? "b1" : "b2";
  }
  tpde::ValLocalIdx val_local_idx(u32 value) const {
    return static_cast<tpde::ValLocalIdx>(value - 10);
  }
  bool val_ignore_in_liveness_analysis(u32 value) const { return value == 15; }
  bool val_is_phi(u32 value) const { return value == 13; }
  Phi val_as_phi(u32) const { return {}; }
  u32 val_alloca_size(u32) const { return 64; }
  u32 val_alloca_align(u32) const { return 16; }
  std::string_view value_fmt_ref(u32 value) const {
    constexpr std::array<std::string_view, 6> names{
        "v10", "v11", "v12", "v13", "v14", "v15"};
    return names[value - 10];
  }
  std::span<const u32> inst_operands(u32 inst) const {
    return inst == 20 ? std::span<const u32>(first_operands)
                      : std::span<const u32>(second_operands);
  }
  std::span<const u32> inst_results(u32 inst) const {
    return inst == 20 ? first_results : second_results;
  }
  bool inst_fused(u32 inst) const { return inst == 21; }
  std::string_view inst_fmt_ref(u32 inst) const {
    return inst == 20 ? "i20" : "i21";
  }
  void start_compile() { ++starts; }
  void end_compile() { ++ends; }
  bool switch_func(u32 func) {
    ++switches;
    if (func != 0)
      return false;
    current = func;
    return true;
  }
  void reset() {
    ++resets;
    current = invalid;
    info.fill(0);
    info2.fill(0);
  }
};

struct Adapter : AdapterNoHighest {
  static constexpr bool TPDE_PROVIDES_HIGHEST_VAL_IDX = true;
  u32 cur_highest_val_idx() const { return 15; }
};

#define MISSING0(name) struct Missing_##name : Adapter { void name() = delete; };
#define MISSING1(name) struct Missing_##name : Adapter { void name(u32) = delete; };
#define MISSING2(name) \
  struct Missing_##name : Adapter { void name(u32, u32) = delete; };
MISSING0(func_count)
MISSING0(funcs)
MISSING0(funcs_to_compile)
MISSING1(func_link_name)
MISSING1(func_extern)
MISSING1(func_only_local)
MISSING1(func_has_weak_linkage)
MISSING0(cur_needs_unwind_info)
MISSING0(cur_is_vararg)
MISSING0(cur_highest_val_idx)
MISSING0(cur_args)
MISSING1(cur_arg_is_byval)
MISSING1(cur_arg_byval_size)
MISSING1(cur_arg_byval_align)
MISSING1(cur_arg_is_sret)
MISSING0(cur_static_allocas)
MISSING0(cur_has_dynamic_alloca)
MISSING0(cur_entry_block)
MISSING0(cur_blocks)
MISSING1(block_succs)
MISSING1(block_insts)
MISSING1(block_phis)
MISSING1(block_info)
MISSING2(block_set_info)
MISSING1(block_info2)
MISSING2(block_set_info2)
MISSING1(block_fmt_ref)
MISSING1(val_local_idx)
MISSING1(val_ignore_in_liveness_analysis)
MISSING1(val_is_phi)
MISSING1(val_as_phi)
MISSING1(val_alloca_size)
MISSING1(val_alloca_align)
MISSING1(value_fmt_ref)
MISSING1(inst_operands)
MISSING1(inst_results)
MISSING1(inst_fused)
MISSING1(inst_fmt_ref)
MISSING0(start_compile)
MISSING0(end_compile)
MISSING1(switch_func)
MISSING0(reset)
#undef MISSING0
#undef MISSING1
#undef MISSING2

#define WRONG_U32(name, params) \
  struct Wrong_##name : Adapter { unsigned long long name params { return 0; } };
WRONG_U32(cur_arg_byval_size, (u32))
WRONG_U32(cur_arg_byval_align, (u32))
WRONG_U32(block_info, (u32))
WRONG_U32(block_info2, (u32))
#undef WRONG_U32
#define WRONG_CONSTANT(name) \
  struct Wrong_##name : Adapter { static constexpr unsigned long long name = 0; };
WRONG_CONSTANT(INVALID_VALUE_REF)
WRONG_CONSTANT(INVALID_BLOCK_REF)
WRONG_CONSTANT(INVALID_FUNC_REF)
WRONG_CONSTANT(TPDE_PROVIDES_HIGHEST_VAL_IDX)
WRONG_CONSTANT(TPDE_LIVENESS_VISIT_ARGS)
#undef WRONG_CONSTANT

struct NoEqualityRef {
  u32 value;
  constexpr NoEqualityRef(u32 value) : value(value) {}
};
struct NoValueEquality : Adapter {
  using IRValueRef = NoEqualityRef;
  static constexpr IRValueRef INVALID_VALUE_REF{invalid};
};
struct NoBlockEquality : Adapter {
  using IRBlockRef = NoEqualityRef;
  static constexpr IRBlockRef INVALID_BLOCK_REF{invalid};
};
struct NoFuncEquality : Adapter {
  using IRFuncRef = NoEqualityRef;
  static constexpr IRFuncRef INVALID_FUNC_REF{invalid};
};
template <unsigned Words> struct Ref {
  u32 words[Words];
  constexpr Ref(u32 value = 0) : words{} { words[0] = value; }
  constexpr operator u32() const { return words[0]; }
  constexpr bool operator==(const Ref &) const = default;
};
struct EightByteRefs : Adapter {
  using IRValueRef = Ref<2>;
  using IRBlockRef = Ref<2>;
  using IRFuncRef = Ref<2>;
  static constexpr IRValueRef INVALID_VALUE_REF{invalid};
  static constexpr IRBlockRef INVALID_BLOCK_REF{invalid};
  static constexpr IRFuncRef INVALID_FUNC_REF{invalid};
};
struct LargeInstRef : Adapter { using IRInstRef = Ref<3>; };
struct NoVisitArgs : Adapter {
  static constexpr bool TPDE_LIVENESS_VISIT_ARGS = false;
};
struct ConvertibleResults : Adapter {
  unsigned long long func_count() const { return 2; }
  unsigned long long cur_highest_val_idx() const { return 15; }
  u32 cur_is_vararg() const { return 0; }
};
struct BigValue : Adapter {
  using IRValueRef = Ref<3>;
  static constexpr IRValueRef INVALID_VALUE_REF{invalid};
};
struct BigBlock : Adapter {
  using IRBlockRef = Ref<3>;
  static constexpr IRBlockRef INVALID_BLOCK_REF{invalid};
};
struct BigFunc : Adapter {
  using IRFuncRef = Ref<3>;
  static constexpr IRFuncRef INVALID_FUNC_REF{invalid};
};
struct BadInstType : Adapter { using IRInstRef = void; };
struct NoDeref { NoDeref &operator++(); bool operator!=(NoDeref) const; };
struct NoIncrement { u32 operator*() const; bool operator!=(NoIncrement) const; };
struct NoCompare { u32 operator*() const; NoCompare &operator++(); };
struct ValueIncrement {
  u32 operator*() const;
  ValueIncrement operator++();
  bool operator!=(ValueIncrement) const;
};
struct Sentinel {};
struct SentinelIter {
  u32 operator*() const;
  SentinelIter &operator++();
  bool operator!=(SentinelIter) const;
  bool operator!=(Sentinel) const;
};
struct SentinelRange { SentinelIter begin(); Sentinel end(); };
struct BadRange { u32 begin(); u32 end(); };
struct BadPhi : Phi { void incoming_val_for_block(u32) = delete; };
struct WrongPhiCount : Phi { void incoming_count(); };
struct WrongPhiValue : Phi { void incoming_val_for_slot(u32); };
struct WrongPhiBlock : Phi { void incoming_block_for_slot(u32); };
struct BadRangeAdapter : Adapter { BadRange cur_blocks(); };
struct BadPhiAdapter : Adapter { BadPhi val_as_phi(u32); };

void concept_result(const char *name, bool result) {
  std::printf("concept %s %u\n", name, unsigned(result));
}

void concepts() {
  concept_result("complete", tpde::IRAdaptor<Adapter>);
  concept_result("no_highest", tpde::IRAdaptor<AdapterNoHighest>);
  concept_result("eight_byte_refs", tpde::IRAdaptor<EightByteRefs>);
  concept_result("large_inst_ref", tpde::IRAdaptor<LargeInstRef>);
  concept_result("no_visit_args", tpde::IRAdaptor<NoVisitArgs>);
  concept_result("convertible_results", tpde::IRAdaptor<ConvertibleResults>);
  concept_result("format_integer", tpde::CanBeFormatted<u32>);
  concept_result("format_string", tpde::CanBeFormatted<std::string_view>);
  concept_result("is_false_false", tpde::IsFalse<false>);
  concept_result("is_false_true", tpde::IsFalse<true>);
  concept_result("iter_pointer", tpde::IRIter<const u32 *, u32>);
  concept_result("iter_value_increment", tpde::IRIter<ValueIncrement, u32>);
  concept_result("iter_no_deref", tpde::IRIter<NoDeref, u32>);
  concept_result("iter_no_increment", tpde::IRIter<NoIncrement, u32>);
  concept_result("iter_no_compare", tpde::IRIter<NoCompare, u32>);
  concept_result("end_sentinel", tpde::IREndIter<Sentinel, u32, SentinelIter>);
  concept_result("end_wrong", tpde::IREndIter<Sentinel, u32, const u32 *>);
  concept_result("range_span", tpde::IRRange<std::span<const u32>, u32>);
  concept_result("range_sentinel", tpde::IRRange<SentinelRange, u32>);
  concept_result("range_bad", tpde::IRRange<BadRange, u32>);
  concept_result("phi", tpde::PHIRef<Phi, u32, u32>);
  concept_result("phi_missing_lookup", tpde::PHIRef<BadPhi, u32, u32>);
  concept_result("phi_wrong_count", tpde::PHIRef<WrongPhiCount, u32, u32>);
  concept_result("phi_wrong_value", tpde::PHIRef<WrongPhiValue, u32, u32>);
  concept_result("phi_wrong_block", tpde::PHIRef<WrongPhiBlock, u32, u32>);
  concept_result("value_no_equality", tpde::IRAdaptor<NoValueEquality>);
  concept_result("block_no_equality", tpde::IRAdaptor<NoBlockEquality>);
  concept_result("func_no_equality", tpde::IRAdaptor<NoFuncEquality>);
  concept_result("value_too_large", tpde::IRAdaptor<BigValue>);
  concept_result("block_too_large", tpde::IRAdaptor<BigBlock>);
  concept_result("func_too_large", tpde::IRAdaptor<BigFunc>);
  concept_result("invalid_inst_type", tpde::IRAdaptor<BadInstType>);
  concept_result("bad_range_adapter", tpde::IRAdaptor<BadRangeAdapter>);
  concept_result("bad_phi_adapter", tpde::IRAdaptor<BadPhiAdapter>);
#define MISSING(name) concept_result("missing_" #name, tpde::IRAdaptor<Missing_##name>);
  MISSING(func_count)
  MISSING(funcs)
  MISSING(funcs_to_compile)
  MISSING(func_link_name)
  MISSING(func_extern)
  MISSING(func_only_local)
  MISSING(func_has_weak_linkage)
  MISSING(cur_needs_unwind_info)
  MISSING(cur_is_vararg)
  MISSING(cur_highest_val_idx)
  MISSING(cur_args)
  MISSING(cur_arg_is_byval)
  MISSING(cur_arg_byval_size)
  MISSING(cur_arg_byval_align)
  MISSING(cur_arg_is_sret)
  MISSING(cur_static_allocas)
  MISSING(cur_has_dynamic_alloca)
  MISSING(cur_entry_block)
  MISSING(cur_blocks)
  MISSING(block_succs)
  MISSING(block_insts)
  MISSING(block_phis)
  MISSING(block_info)
  MISSING(block_set_info)
  MISSING(block_info2)
  MISSING(block_set_info2)
  MISSING(block_fmt_ref)
  MISSING(val_local_idx)
  MISSING(val_ignore_in_liveness_analysis)
  MISSING(val_is_phi)
  MISSING(val_as_phi)
  MISSING(val_alloca_size)
  MISSING(val_alloca_align)
  MISSING(value_fmt_ref)
  MISSING(inst_operands)
  MISSING(inst_results)
  MISSING(inst_fused)
  MISSING(inst_fmt_ref)
  MISSING(start_compile)
  MISSING(end_compile)
  MISSING(switch_func)
  MISSING(reset)
#undef MISSING
#define WRONG(name) concept_result("wrong_" #name, tpde::IRAdaptor<Wrong_##name>);
  WRONG(cur_arg_byval_size)
  WRONG(cur_arg_byval_align)
  WRONG(block_info)
  WRONG(block_info2)
  WRONG(INVALID_VALUE_REF)
  WRONG(INVALID_BLOCK_REF)
  WRONG(INVALID_FUNC_REF)
  WRONG(TPDE_PROVIDES_HIGHEST_VAL_IDX)
  WRONG(TPDE_LIVENESS_VISIT_ARGS)
#undef WRONG
}

template <tpde::IRAdaptor A> void trace(A &a) {
  std::printf("refs %zu %zu %zu %zu\n", sizeof(typename A::IRValueRef),
              sizeof(typename A::IRInstRef), sizeof(typename A::IRBlockRef),
              sizeof(typename A::IRFuncRef));
  std::printf("sentinels %u %u %u\n", A::INVALID_VALUE_REF,
              A::INVALID_BLOCK_REF, A::INVALID_FUNC_REF);
  std::printf("flags %u %u\n", unsigned(A::TPDE_PROVIDES_HIGHEST_VAL_IDX),
              unsigned(A::TPDE_LIVENESS_VISIT_ARGS));
  std::printf("module %u\n", a.func_count());
  for (auto f : a.funcs()) {
    auto name = a.func_link_name(f);
    std::printf("func %u %.*s %u %u %u\n", f, int(name.size()), name.data(),
                unsigned(a.func_extern(f)), unsigned(a.func_only_local(f)),
                unsigned(a.func_has_weak_linkage(f)));
  }
  a.start_compile();
  for (auto f : a.funcs_to_compile()) {
    std::printf("compile %u\n", f);
    std::printf("switch %u %u\n", f, unsigned(a.switch_func(f)));
  }
  std::printf("current %u %u %u %u %u\n", unsigned(a.cur_needs_unwind_info()),
              unsigned(a.cur_is_vararg()), a.cur_highest_val_idx(),
              unsigned(a.cur_has_dynamic_alloca()), a.cur_entry_block());
  u32 index = 0;
  for (auto value : a.cur_args()) {
    std::printf("arg %u %u %u %u %u %u\n", index, value,
                unsigned(a.cur_arg_is_byval(index)), a.cur_arg_byval_size(index),
                a.cur_arg_byval_align(index), unsigned(a.cur_arg_is_sret(index)));
    ++index;
  }
  for (auto value : a.cur_static_allocas())
    std::printf("alloca %u %u %u\n", value, a.val_alloca_size(value),
                a.val_alloca_align(value));
  for (auto block : a.cur_blocks()) {
    a.block_set_info(block, 100 + block);
    a.block_set_info2(block, 200 + block);
    auto name = std::format("{}", a.block_fmt_ref(block));
    std::printf("block %u %u %u %s\n", block, a.block_info(block),
                a.block_info2(block), name.c_str());
    for (auto successor : a.block_succs(block))
      std::printf("succ %u %u\n", block, successor);
    for (auto value : a.block_phis(block)) {
      auto phi = a.val_as_phi(value);
      std::printf("phi %u %u\n", value, phi.incoming_count());
      for (u32 slot = 0; slot < phi.incoming_count(); ++slot) {
        auto predecessor = phi.incoming_block_for_slot(slot);
        std::printf("incoming %u %u %u %u\n", slot,
                    phi.incoming_val_for_slot(slot), predecessor,
                    phi.incoming_val_for_block(predecessor));
      }
    }
    for (auto inst : a.block_insts(block)) {
      auto name = std::format("{}", a.inst_fmt_ref(inst));
      std::printf("inst %u %u %s\n", inst, unsigned(a.inst_fused(inst)), name.c_str());
      for (auto operand : a.inst_operands(inst))
        std::printf("operand %u %u\n", inst, operand);
      for (auto result : a.inst_results(inst))
        std::printf("result %u %u\n", inst, result);
    }
  }
  for (u32 value = 10; value <= 15; ++value) {
    auto name = std::format("{}", a.value_fmt_ref(value));
    std::printf("value %u %u %u %u %s\n", value, u32(a.val_local_idx(value)),
                unsigned(a.val_ignore_in_liveness_analysis(value)),
                unsigned(a.val_is_phi(value)), name.c_str());
  }
  std::printf("switch 1 %u\n", unsigned(a.switch_func(1)));
  a.end_compile();
  a.reset();
  std::printf("reset %u %u %u %u %u\n", a.block_info(1), a.block_info2(1),
              a.block_info(2), a.block_info2(2), a.current);
  a.start_compile();
  std::printf("recompile %u\n", unsigned(a.switch_func(0)));
  a.end_compile();
  std::printf("lifecycle %u %u %u %u\n", a.starts, a.ends, a.switches, a.resets);
}

int main() {
  concepts();
  Adapter adapter;
  trace(adapter);
}
