// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <bit>
#include <concepts>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <iterator>
#include <limits>
#include <memory>
#include <optional>
#include <span>
#include <sstream>
#include <stdexcept>
#include <string>
#include <string_view>
#include <tuple>
#include <utility>
#include <variant>
#include <vector>
#include "tpde/AssemblerElf.hpp"
#include "tpde/x64/CompilerX64.hpp"
#include "tpde/src/test/TestIRAdaptor.hpp"

using namespace tpde;
using tpde::x64::AsmReg;
using tpde::x64::CCAssignerSysV;
using TestIR = tpde::test::TestIR;

template <typename... Values>
static void record(std::string_view tag, Values... values) {
  std::cout << tag;
  ((std::cout << ' ' << u64(values)), ...);
  std::cout << '\n';
}

static void blob(std::string_view tag, std::span<const u8> bytes) {
  static constexpr char digits[] = "0123456789abcdef";
  std::cout << tag << ' ';
  for (u8 byte : bytes) std::cout << digits[byte >> 4] << digits[byte & 15];
  std::cout << '\n';
}

static u64 number(std::istream &input) {
  u64 result;
  if (!(input >> result)) throw std::runtime_error("missing decimal operand");
  return result;
}

static CCAssignment cc_assignment(std::istream &input) {
  CCAssignment result;
  result.bank = RegBank{u8(number(input))};
  result.size = number(input);
  result.align = number(input);
  result.consecutive = number(input);
  result.byval = number(input);
  result.sret = number(input);
  return result;
}

static void cc_script(std::istream &input, bool vararg) {
  CCAssignerSysV assigner{vararg};
  std::string command;
  while (input >> command) {
    if (command == "ccarg" || command == "ccret") {
      auto assignment = cc_assignment(input);
      if (command == "ccarg") assigner.assign_arg(assignment);
      else assigner.assign_ret(assignment);
      record(command, assignment.reg.id(), assignment.stack_off,
             assignment.bank.id(), assignment.size, assignment.align,
             assignment.consecutive, assignment.byval, assignment.sret,
             assigner.get_stack_size(), assigner.is_vararg());
    } else if (command == "ccreset") {
      assigner.reset();
      record("ccreset", assigner.get_stack_size(), assigner.is_vararg());
    } else if (command == "ccinfo") {
      const auto &info = assigner.get_ccinfo();
      record("ccinfo", info.allocatable_regs, info.callee_saved_regs,
             info.arg_regs, info.red_zone_size);
    } else throw std::runtime_error("unknown calling-convention command");
  }
}

struct Adaptor : tpde::test::TestIRAdaptor {
  bool vararg, dynamic;
  Adaptor(TestIR *ir, bool vararg, bool dynamic)
      : TestIRAdaptor(ir), vararg(vararg), dynamic(dynamic) {}
  bool cur_is_vararg() const { return vararg; }
  bool cur_has_dynamic_alloca() const { return dynamic; }
};

struct Oracle : tpde::x64::CompilerX64<Adaptor, Oracle> {
  using Parent = tpde::x64::CompilerX64<Adaptor, Oracle>;
  using Parent::Parent;
  struct Parts {
    u32 count() const { return 1; }
    u32 size_bytes(u32) const { return 8; }
    RegBank reg_bank(u32) const { return RegBank{0}; }
  };
  bool leaf = true;
  bool emitted_return = false;
  bool reset_requested = false;
  std::istream *input = nullptr;
  std::vector<Label> labels;
  std::array<std::unique_ptr<ScratchReg>, 64> held;
  std::unique_ptr<CCAssignerSysV> call_assigner;
  std::unique_ptr<CallBuilder> call_builder;
  SymRef tls_symbol;
  ValueAssignment *stack_call_target = nullptr;

  Parts val_parts(IRValueRef) const { return {}; }
  bool cur_func_may_emit_calls() const { return !leaf; }
  SymRef cur_personality_func() const { return {}; }
  bool try_force_fixed_assignment(IRValueRef) const { return false; }
  std::optional<ValRefSpecial> val_ref_special(IRValueRef) const { return {}; }
  ValuePart val_part_ref_special(ValRefSpecial &, u32) { std::abort(); }
  void define_func_idx(IRFuncRef, u32) {}
  bool compile_inst(IRInstRef, InstRange);

  ValueAssignment *make_assignment(u32 count, RegBank bank, u32 size, bool owned = true) {
    auto *value = assignments.allocator.allocate(count);
    value->frame_off = 0;
    value->part_count = count;
    value->references_left = owned ? 1 : 2;
    value->max_part_size = size;
    value->pending_free = false;
    value->variable_ref = false;
    value->stack_variable = false;
    value->delay_free = false;
    for (u32 index = 0; index < count; ++index) {
      AssignmentPartRef part{value, index};
      part.reset();
      part.set_bank(bank);
      part.set_part_size(size);
    }
    if (assignments.value_ptrs.empty()) assignments.value_ptrs.resize(1, nullptr);
    if (assignments.value_ptrs[0]) throw std::runtime_error("fixture assignment collision");
    assignments.value_ptrs[0] = value;
    return value;
  }

  std::variant<SymRef, ValuePart> call_target(u32 kind, u64 value) {
    if (kind == 0) {
      auto name = std::string("external") + std::to_string(value);
      return assembler.sym_add_undef(name, Assembler::SymBinding::GLOBAL);
    }
    if (kind == 3) {
      if (stack_call_target) throw std::runtime_error("nested stack call target");
      stack_call_target = make_assignment(1, RegBank{0}, 8, false);
      stack_call_target->frame_off = -32;
      AssignmentPartRef{stack_call_target, 0}.set_stack_valid();
      stack.frame_used = true;
      return ValuePart{ValLocalIdx{0}, stack_call_target, 0, false};
    }
    if (kind > 2) throw std::runtime_error("invalid call target kind");
    ValuePart part{value, 8, RegBank{0}};
    if (kind == 2) part.load_to_specific(this, AsmReg::R11);
    return part;
  }

  void release_call_target() {
    if (!stack_call_target) return;
    stack_call_target->frame_off = 0;
    release_assignment(ValLocalIdx{0}, stack_call_target);
    stack_call_target = nullptr;
  }

  void state() {
    record("state", text_writer.offset(), stack.frame_size, stack.frame_used,
           stack.generated_call, stack.has_dynamic_alloca, stack.is_leaf_function,
           register_file.allocatable, register_file.used, register_file.clobbered,
           preserve_flags, max_callee_stack_arg_size, scalar_arg_count,
           vec_arg_count, reg_save_frame_off, var_arg_stack_off,
           sym_tls_get_addr.valid());
  }

  void expression(std::istream &input) {
    const u32 base_kind = number(input);
    const u32 base_id = number(input);
    const u32 index_kind = number(input);
    const u32 index_id = number(input);
    const i64 scale = i64(number(input));
    const i64 displacement = i64(number(input));
    GenericValuePart::Expr expression;
    expression.base = AsmReg::make_invalid();
    expression.index = AsmReg::make_invalid();
    const auto operand = [&](u32 kind, u32 id) -> std::variant<AsmReg, ScratchReg> {
      if (kind == 0) return AsmReg::make_invalid();
      if (kind == 1) return AsmReg{u64(id)};
      if (kind != 2) throw std::runtime_error("invalid expression operand kind");
      ScratchReg scratch{this};
      scratch.alloc_specific(AsmReg{u64(id)});
      return scratch;
    };
    expression.base = operand(base_kind, base_id);
    expression.index = operand(index_kind, index_id);
    expression.scale = scale;
    expression.disp = displacement;
    GenericValuePart generic{std::move(expression)};
    record("expr", gval_expr_as_reg(generic).id());
    generic.reset();
  }

  void proarg(std::istream &input) {
    const u32 mode = number(input);
    const RegBank bank{u8(number(input))};
    const u32 size = number(input);
    const u32 count = number(input);
    const u32 reg = number(input);
    const u32 stack_off = number(input);
    const bool byval = number(input);
    auto *value = make_assignment(count, bank, size, mode == 0);
    AssignmentPartRef assignment{value, 0};
    if (mode == 2) {
      auto fixed = register_file.find_first_free_excluding(bank, 0);
      if (!fixed.valid()) throw std::runtime_error("no register for fixed proarg");
      assignment.set_reg(fixed);
      assignment.set_fixed_assignment(true);
      assignment.set_register_valid(true);
      register_file.mark_used(fixed, ValLocalIdx{0}, 0);
      register_file.mark_clobbered(fixed);
      register_file.mark_fixed(fixed);
      ++assignments.cur_fixed_assignment_count[bank.id()];
    }
    ValuePart part{ValLocalIdx{0}, value, 0, mode == 0};
    CCAssignment cca;
    cca.bank = bank;
    cca.size = size;
    cca.reg = Reg{reg};
    cca.stack_off = stack_off;
    cca.byval = byval;
    auto result = prologue_assign_arg_part(std::move(part), cca);
    record("proarg", result.has_value(), result.value_or(0), value->frame_off,
           assignment.register_valid(), assignment.stack_valid(),
           assignment.get_reg().id(), part.has_reg());
    part.reset(this);
    release_assignment(ValLocalIdx{0}, value);
  }

  void command(std::string_view op, std::istream &input) {
    const auto n = [&] { return number(input); };
    if (op == "begin" || op == "finish" || op == "dump") return;
    if (op == "reset") { reset_requested = true; return; }
    if (op == "state") { state(); return; }
    if (op == "flags") { set_preserve_flags(n()); return; }
    if (op == "features") { record("features", has_cpu_feats(CPU_FEATURES(n()))); return; }
    if (op == "frame") {
      stack.frame_size = n(); stack.frame_used = n(); stack.generated_call = n();
      return;
    }
    if (op == "regs") { register_file.allocatable = n(); return; }
    if (op == "clobber") { register_file.clobbered |= n(); return; }
    if (op == "hold") {
      const auto id = n();
      if (id >= held.size() || held[id]) throw std::runtime_error("invalid held register");
      held[id] = std::make_unique<ScratchReg>(this);
      held[id]->alloc_specific(AsmReg{id});
      return;
    }
    if (op == "release") {
      const auto id = n();
      if (id >= held.size() || !held[id]) throw std::runtime_error("register not held");
      held[id].reset(); return;
    }
    if (op == "mov") {
      const AsmReg dst{n()}, src{n()}; const u32 size = n();
      mov(dst, src, size); return;
    }
    if (op == "spill" || op == "load") {
      const AsmReg reg{n()}; const i32 off = i32(n()); const u32 size = n();
      if (op == "spill") spill_reg(reg, off, size);
      else load_from_stack(reg, off, size, n());
      return;
    }
    if (op == "stackaddr" || op == "spillslot") {
      const AsmReg dst = op == "stackaddr" ? AsmReg{n()} : AsmReg{};
      const i32 off = i32(n());
      auto *value = make_assignment(1, RegBank{0}, 8);
      value->frame_off = off;
      AssignmentPartRef assignment{value, 0};
      if (op == "stackaddr") {
        value->variable_ref = value->stack_variable = true;
        load_address_of_stack_var(dst, assignment);
      } else {
        assignment.set_stack_valid();
        auto generic = val_spill_slot(assignment);
        const auto &expression = std::get<GenericValuePart::Expr>(generic.state);
        record("spillslot", expression.base_reg().id(), expression.scale, expression.disp);
      }
      value->frame_off = 0;
      release_assignment(ValLocalIdx{0}, value);
      return;
    }
    if (op == "constant") {
      const RegBank bank{u8(n())}; const u32 size = n(); const AsmReg dst{n()};
      const std::array<u64, 2> data{n(), n()};
      materialize_constant(data.data(), bank, size, dst); return;
    }
    if (op == "intext") {
      const AsmReg dst{n()}, src{n()}; const bool sign = n();
      const u32 from = n(), to = n(); generate_raw_intext(dst, src, sign, from, to); return;
    }
    if (op == "bfi" || op == "bfiz") {
      const AsmReg dst{n()}, src{n()}; const u32 lsb = n(), width = n();
      if (op == "bfi") generate_raw_bfi(dst, src, lsb, width);
      else generate_raw_bfiz(dst, src, lsb, width);
      return;
    }
    if (op == "invert" || op == "swap" || op == "cond") {
      const auto cc = Jump(n());
      if (op == "invert") record(op, u32(invert_jump(cc)));
      else if (op == "swap") record(op, u32(swap_jump(cc)));
      else record(op, u32(jump_to_cond(cc)));
      return;
    }
    if (op == "label") {
      record("label", labels.size()); labels.push_back(text_writer.label_create()); return;
    }
    if (op == "place") { text_writer.label_place(labels.at(n()), text_writer.offset()); return; }
    if (op == "jump") {
      const auto cc = Jump(n()); const auto target = labels.at(n());
      generate_raw_jump(cc, target); return;
    }
    if (op == "set" || op == "mask" || op == "cmov") {
      const auto cc = Jump(n()); const AsmReg dst{n()};
      if (op == "set") generate_raw_set(cc, dst, n());
      else if (op == "mask") generate_raw_mask(cc, dst);
      else { const AsmReg src{n()}; generate_raw_cmov(cc, dst, src, n()); }
      return;
    }
    if (op == "cmpeq" || op == "binary") {
      const auto equal = labels.at(n());
      const auto greater = op == "binary" ? labels.at(n()) : Label(0);
      const AsmReg cmp{n()}, tmp{n()}; const u64 value = n(); const bool width32 = n();
      if (op == "cmpeq") switch_emit_cmpeq(equal, cmp, tmp, value, width32);
      else switch_emit_binary_step(equal, greater, cmp, tmp, value, width32);
      return;
    }
    if (op == "table") {
      const auto fallback = labels.at(n()); const AsmReg cmp{n()}, tmp{n()};
      const u64 low = n(), high = n(); const bool width32 = n();
      if (high < low || high - low > 4095) throw std::runtime_error("invalid table fixture range");
      auto *table = switch_create_jump_table(fallback, cmp, tmp, low, high, width32);
      if (!table) throw std::runtime_error("unexpected absent x64 jump table");
      for (auto &label : table->labels()) label = labels.at(n());
      return;
    }
    if (op == "expr") { expression(input); return; }
    if (op == "afixed") {
      const u64 size = n(); const u32 align = n(); ValuePart result{RegBank{0}};
      alloca_fixed(size, align, result); record(op, result.cur_reg().id());
      result.reset(this); return;
    }
    if (op == "adynamic_ref") {
      const u64 size = n(), value = n(); const u32 align = n(), reg = n();
      auto *assignment = make_assignment(1, RegBank{0}, 8, false);
      ValuePart count{ValLocalIdx{0}, assignment, 0, false}, result{RegBank{0}};
      if (reg == 255) count.alloc_reg(this);
      else count.alloc_specific(this, AsmReg{u64(reg)});
      materialize_constant(&value, RegBank{0}, 8, count.cur_reg());
      alloca_dynamic(size, std::move(count), align, result);
      record(op, result.cur_reg().id());
      result.reset(this); count.reset(this);
      release_assignment(ValLocalIdx{0}, assignment);
      return;
    }
    if (op == "adynamic") {
      const u64 size = n(), value = n(); const u32 align = n(), reg = n();
      ValuePart count{value, 8, RegBank{0}}, result{RegBank{0}};
      if (reg != 255) count.load_to_specific(this, AsmReg{u64(reg)});
      alloca_dynamic(size, std::move(count), align, result);
      record(op, result.cur_reg().id()); result.reset(this); count.reset(this); return;
    }
    if (op == "fixed") {
      const RegBank bank{u8(n())}; fixed_assignment_nonallocatable_mask = n();
      auto *value = make_assignment(1, bank, 8);
      record(op, select_fixed_assignment_reg(AssignmentPartRef{value, 0}, IRValueRef(0)).id());
      release_assignment(ValLocalIdx{0}, value); return;
    }
    if (op == "proarg") { proarg(input); return; }
    if (op == "proend") {
      const u32 scalars = n(), vectors = n();
      if (scalars > 64 || vectors > 64) throw std::runtime_error("oversize proend fixture");
      CCAssignerSysV assigner;
      u64 assigned_regs = 0;
      for (u32 bank = 0; bank < 2; ++bank) {
        const u32 count = bank == 0 ? scalars : vectors;
        for (u32 index = 0; index < count; ++index) {
          CCAssignment assignment;
          assignment.bank = RegBank{u8(bank)};
          assignment.size = assignment.align = 8;
          assigner.assign_arg(assignment);
          if (assignment.reg.valid()) assigned_regs |= u64{1} << assignment.reg.id();
        }
      }
      const auto previous = register_file.allocatable;
      const auto &info = assigner.get_ccinfo();
      register_file.allocatable = (info.allocatable_regs & ~info.arg_regs) | assigned_regs;
      prologue_end(&assigner);
      register_file.allocatable = previous;
      return;
    }
    if (op == "epilog") { gen_func_epilog(); emitted_return = true; return; }
    if (op == "pad") {
      const u64 count = n(); const u8 value = n();
      if (count > 1048576) throw std::runtime_error("oversize padding fixture");
      text_writer.ensure_space(count);
      std::memset(text_writer.cur_ptr(), value, count); text_writer.cur_ptr() += count;
      return;
    }
    if (op == "callbegin") {
      if (call_builder) throw std::runtime_error("nested call builder");
      call_assigner = std::make_unique<CCAssignerSysV>(bool(n()));
      call_builder = std::make_unique<CallBuilder>(*this, *call_assigner);
      return;
    }
    if (op == "callarg" || op == "callarg_loaded") {
      if (!call_builder) throw std::runtime_error("no call builder");
      auto assignment = cc_assignment(input); assignment.int_ext = n();
      const std::array<u64, 2> data{n(), n()};
      ValuePart part{data.data(), assignment.byval ? 8 : assignment.size,
                     assignment.byval ? RegBank{0} : assignment.bank};
      if (op == "callarg_loaded") part.load_to_reg(this);
      call_builder->add_arg(std::move(part), assignment); return;
    }
    if (op == "calltarget" || op == "generatecall") {
      const u32 kind = n(); const u64 value = n();
      if (op == "calltarget") {
        if (!call_builder) throw std::runtime_error("no call builder");
        call_builder->call(call_target(kind, value));
      } else {
        const bool vararg = n();
        generate_call(call_target(kind, value), {}, nullptr, vararg);
      }
      release_call_target();
      return;
    }
    if (op == "callret") {
      if (!call_builder) throw std::runtime_error("no call builder");
      const RegBank bank{u8(n())}; const u32 size = n();
      auto *value = make_assignment(1, bank, size);
      ValuePart part{ValLocalIdx{0}, value, 0, true};
      call_builder->add_ret(part, CCAssignment{});
      record(op, part.cur_reg_unlocked().id());
      part.reset(this); release_assignment(ValLocalIdx{0}, value); return;
    }
    if (op == "callend") {
      if (!call_builder) throw std::runtime_error("no call builder");
      call_builder.reset(); call_assigner.reset(); return;
    }
    if (op == "tls" || op == "tlslive") {
      const u64 reg = op == "tlslive" ? n() : 0;
      const u64 value = op == "tlslive" ? n() : 0;
      const auto model = TLSModel(n());
      if (!tls_symbol.valid()) tls_symbol = assembler.sym_add_undef("tls", Assembler::SymBinding::GLOBAL);
      if (op == "tlslive") {
        const RegBank bank{u8(reg < 32 ? 0 : 1)};
        auto *assignment = make_assignment(1, bank, 8, false);
        ValuePart part{ValLocalIdx{0}, assignment, 0, false};
        part.alloc_specific(this, AsmReg{reg});
        materialize_constant(&value, bank, 8, part.cur_reg());
        part.reset(this);
        auto result = tls_get_addr(tls_symbol, model);
        const auto result_reg = result.cur_reg();
        result.reset();
        AssignmentPartRef state{assignment, 0};
        const bool valid = state.register_valid(), spilled = state.stack_valid();
        const i32 offset = assignment->frame_off;
        ValuePart reload{ValLocalIdx{0}, assignment, 0, false};
        reload.load_to_reg(this);
        record(op, valid, spilled, offset, result_reg.id(), reload.cur_reg().id());
        reload.reset(this);
        release_assignment(ValLocalIdx{0}, assignment);
      } else {
        auto result = tls_get_addr(tls_symbol, model); record(op, result.cur_reg().id());
      }
      return;
    }
    throw std::runtime_error(std::string("unknown operation: ") + std::string(op));
  }
};

bool Oracle::compile_inst(IRInstRef instruction, InstRange) {
  if (adaptor->ir->values[u32(instruction)].op == TestIR::Value::Op::any) return true;
  std::string op;
  while (*input >> op) command(op, *input);
  if (call_builder) throw std::runtime_error("unclosed call builder");
  for (const auto &reg : held) if (reg) throw std::runtime_error("unreleased scratch register");
  if (!emitted_return) gen_func_epilog();
  release_regs_after_return();
  return true;
}

int main() {
  try {
    std::string command;
    if (!(std::cin >> command)) throw std::runtime_error("empty operation script");
    if (command == "sanitizer") {
      record("sanitizer", tpde::util::address_sanitizer_active); return 0;
    }
    if (command == "ccinit") { const bool vararg = number(std::cin); cc_script(std::cin, vararg); return 0; }
    if (command != "init") throw std::runtime_error("script must begin with init or ccinit");
    const auto features = Oracle::CPU_FEATURES(number(std::cin));
    const bool vararg = number(std::cin), dynamic = number(std::cin), leaf = number(std::cin);
    TestIR ir;
    if (!ir.parse_ir("foo() {\nentry:\n%fixture = any\nterminate\n}\n")) throw std::runtime_error("dummy TestIR parse failed");
    Adaptor adaptor{&ir, vararg, dynamic};
    Oracle compiler{&adaptor, features};
    compiler.leaf = leaf;
    compiler.input = &std::cin;
    adaptor.start_compile();
    const bool success = compiler.compile();
    adaptor.end_compile();
    record("compile", success);
    if (!success) return 1;
    auto text = compiler.assembler.get_default_section(SectionKind::Text);
    const auto &section = compiler.assembler.get_section(text);
    blob("text", {section.data.data(), section.data.size()});
    auto object = compiler.assembler.build_object_file();
    blob("object", {object.data(), object.size()});
    if (compiler.reset_requested) {
      compiler.reset();
      record("reset", compiler.func_ret_offs.size(), compiler.sym_tls_get_addr.valid(),
             compiler.func_syms.size(), compiler.block_labels.size(), compiler.personality_syms.size());
    }
    return 0;
  } catch (const std::exception &error) {
    std::cerr << "compiler-x64 oracle: " << error.what() << '\n';
    return 2;
  }
}
