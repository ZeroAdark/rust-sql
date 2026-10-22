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
#include "tpde/CompilerBase.hpp"
#include "tpde/src/test/TestIRAdaptor.hpp"

using namespace tpde;
using TestIR = tpde::test::TestIR;

template <typename... Values>
void record(std::string_view tag, Values... values) {
  std::cout << tag;
  ((std::cout << ' ' << u64(values)), ...);
  std::cout << '\n';
}

struct Adaptor : tpde::test::TestIRAdaptor {
  using TestIRAdaptor::TestIRAdaptor;
  static constexpr u32 max_static_alloca_size = 256;
  bool val_ignore_in_liveness_analysis(IRValueRef value) const {
    return TestIRAdaptor::val_ignore_in_liveness_analysis(value) &&
           val_alloca_size(value) <= max_static_alloca_size && val_alloca_align(value) <= 16;
  }
  bool cur_arg_is_byval(u32 index) const {
    return ir->values[ir->functions[cur_func].arg_begin_idx + index].name.starts_with("byval_");
  }
  u32 cur_arg_byval_align(u32) const { return 8; }
  u32 cur_arg_byval_size(u32) const { return 24; }
  bool cur_arg_is_sret(u32 index) const {
    return ir->values[ir->functions[cur_func].arg_begin_idx + index].name.starts_with("sret_");
  }
};

struct Writer : FunctionWriter<Writer> {
  static constexpr std::array<u8, 3> instructions{0x0c, 31, 0};
  static constexpr TargetCIEInfo cie{{instructions}, 30, 1, 0x78};
  Writer() : FunctionWriter(cie) {}
  using FunctionWriterBase::alloc_jump_table;
  void handle_fixups() {
    record("writer.finish", offset(), jump_tables.size(), label_offsets.size());
    for (u32 label = 0; label < label_offsets.size(); ++label)
      record("writer.label", label, label_offsets[label]);
    for (auto *table : jump_tables) {
      record("table", table->size, table->idx.id(), table->tmp.id());
      for (auto label : table->labels()) record("table.label", u32(label));
    }
  }
};

struct Assigner : CCAssigner {
  static constexpr u64 all_regs = 0x0000ffff0000ffffull;
  static constexpr CCInfo info{all_regs, 0x0000ff000000ff00ull, 0x0000000f0000000full, 32};
  u32 next[2]{}, returns[2]{}, stack_size = 0;
  Assigner() : CCAssigner(info) {}
  void reset() override { next[0] = next[1] = returns[0] = returns[1] = stack_size = 0; }
  void assign_arg(CCAssignment &assignment) override {
    const u32 bank = assignment.byval ? 0 : assignment.bank.id();
    assert(bank < 2);
    if (assignment.sret) {
      assignment.reg = Reg{3};
      next[0] = 4;
    } else if (!assignment.byval && next[bank] + u32(assignment.consecutive) + 1 <= 4) {
      assignment.reg = Reg{32 * bank + next[bank]++};
    } else {
      if (!assignment.byval) next[bank] = 4;
      const u32 align = std::max<u32>(assignment.align, 1);
      stack_size = util::align_up(stack_size, align);
      assignment.reg = Reg::make_invalid();
      assignment.stack_off = stack_size;
      stack_size += assignment.size;
    }
  }
  u32 get_stack_size() override { return stack_size; }
  void assign_ret(CCAssignment &assignment) override {
    const u32 bank = assignment.bank.id();
    assert(bank < 2 && returns[bank] < 4);
    assignment.reg = Reg{bank * 32 + returns[bank]++};
  }
};

struct RecordingReg : Reg {
  constexpr explicit RecordingReg() : Reg(u8(0xFF)) {}
  constexpr RecordingReg(const Reg base) : Reg(base) {}
  constexpr explicit RecordingReg(const u64 id) : Reg(id) {}
};

template <bool Negative> struct Config : CompilerConfigDefault {
  static constexpr bool FRAME_INDEXING_NEGATIVE = Negative;
  static constexpr u32 PLATFORM_POINTER_SIZE = 8;
  static constexpr u32 NUM_BANKS = 2;
  static constexpr RegBank GP_BANK{0};
  using DefaultCCAssigner = Assigner;
  using Assembler = elf::AssemblerElfA64;
  using AsmReg = RecordingReg;
  using FunctionWriter = Writer;
};

template <bool Negative>
struct RecordingCompiler : CompilerBase<Adaptor, RecordingCompiler<Negative>, Config<Negative>> {
  using Base = CompilerBase<Adaptor, RecordingCompiler<Negative>, Config<Negative>>;
  using typename Base::IRValueRef;
  using typename Base::IRInstRef;
  using typename Base::IRBlockRef;
  using typename Base::IRFuncRef;
  using typename Base::InstRange;
  using typename Base::ValRefSpecial;
  using typename Base::ValueRef;
  using typename Base::ValuePart;
  using typename Base::ValuePartRef;
  using typename Base::ScratchReg;
  using typename Base::GenericValuePart;
  using typename Base::CallArg;
  using typename Base::RetBuilder;
  using Base::adaptor;
  using Base::register_file;
  using Base::stack;
  using Base::text_writer;
  using Base::assembler;
  using Base::cur_cc_assigner;
  static constexpr u32 NUM_FIXED_ASSIGNMENTS[2]{4, 4};
  static constexpr u32 MaxStaticAllocaSize = Adaptor::max_static_alloca_size;
  enum class Jump { jmp, equal, unequal };
  bool fixed = true, jump_tables = true, emit_bytes = false;
  bool spill_scratch = false, inside_spill = false;
  u64 switch_stride = 1;
  u32 function_begin = 0;

  explicit RecordingCompiler(Adaptor *adaptor) : Base(adaptor) {}

  template <typename... Values> void emit(std::string_view tag, Values... values) {
    record(tag, values...);
    if (emit_bytes) text_writer.template write<u8>(0);
  }

  struct Parts {
    u32 parts;
    RegBank bank;
    u32 count() const { return parts; }
    u32 size_bytes(u32) const { return 8; }
    RegBank reg_bank(u32) const { return bank; }
  };
  Parts val_parts(IRValueRef value) {
    const auto &name = adaptor->ir->values[u32(value)].name;
    u32 parts = name.starts_with("p2_") ? 2 : name.starts_with("p3_") ? 3 : 1;
    return {parts, RegBank{u8(name.starts_with("v_") ? 1 : 0)}};
  }
  bool cur_func_may_emit_calls() const { return adaptor->ir->functions[adaptor->cur_func].has_call; }
  SymRef cur_personality_func() const { return {}; }
  std::optional<ValRefSpecial> val_ref_special(IRValueRef) { return {}; }
  ValuePart val_part_ref_special(ValRefSpecial &, u32) { std::abort(); }
  void define_func_idx(IRFuncRef function, u32 index) { record("function.symbol", u32(function), index); }
  bool try_force_fixed_assignment(IRValueRef value) const {
    return adaptor->ir->values[u32(value)].force_fixed_assignment;
  }
  RecordingReg select_fixed_assignment_reg(AssignmentPartRef part, IRValueRef) {
    if (!fixed) return Reg::make_invalid();
    return register_file.find_first_free_excluding(part.bank(), ~0x00000f0000000f00ull);
  }
  void start_func(u32 index) {
    stack.frame_size = 16;
    function_begin = text_writer.offset();
    emit("function.start", index);
  }
  void finish_func(u32 index) {
    snapshot("function.state");
    const u32 size = text_writer.offset() - function_begin;
    assembler.sym_def(this->func_syms[index], text_writer.get_sec_ref(), function_begin, size);
    record("function.end", index, size);
  }
  void prologue_begin(CCAssigner *) { emit("prologue.begin"); }
  void prologue_end(CCAssigner *assigner) { emit("prologue.end", assigner->get_stack_size()); }
  std::optional<i32> prologue_assign_arg_part(ValuePart &&part, CCAssignment assignment) {
    emit("prologue.part", part.local_idx(), part.part(), assignment.reg.id(), assignment.stack_off,
         assignment.size, assignment.byval, assignment.sret);
    if (assignment.byval) return i32(64 + assignment.stack_off);
    if (assignment.reg.valid()) {
      part.set_value_reg(this, assignment.reg);
      register_file.allocatable |= u64{1} << assignment.reg.id();
    } else {
      const auto reg = part.alloc_reg(this);
      emit("prologue.load", reg.id(), assignment.stack_off, assignment.size);
    }
    return {};
  }
  void mov(Reg destination, Reg source, u32 size) { emit("move", destination.id(), source.id(), size); }
  void spill_reg(Reg reg, u32 offset, u32 size) {
    if (spill_scratch && !inside_spill && reg == Reg{7}) {
      inside_spill = true;
      ScratchReg scratch{this};
      scratch.alloc_specific(Reg{2});
      emit("spill.scratch", scratch.cur_reg().id());
      inside_spill = false;
    }
    emit("spill", reg.id(), offset, size);
  }
  void load_from_stack(Reg reg, u32 offset, u32 size) { emit("reload", reg.id(), offset, size); }
  void load_address_of_stack_var(Reg reg, AssignmentPartRef part) {
    emit("stack.address", reg.id(), part.variable_stack_off());
  }
  void load_address_of_var_reference(Reg reg, AssignmentPartRef part) {
    emit("variable.address", reg.id(), part.variable_ref_data());
  }
  void materialize_constant(const u64 *words, RegBank bank, u32 size, Reg reg) {
    emit("constant", bank.id(), size, reg.id());
    for (u32 i = 0; i < (size + 7) / 8; ++i) record("constant.word", words[i]);
  }
  void generate_raw_intext(Reg destination, Reg source, bool sign, u32 from, u32 to) {
    emit("extend", destination.id(), source.id(), sign, from, to);
  }
  RecordingReg gval_expr_as_reg(GenericValuePart &value) {
    auto &expr = std::get<typename GenericValuePart::Expr>(value.state);
    const Reg base = expr.has_base() ? Reg{expr.base_reg()} : Reg::make_invalid();
    const Reg index = expr.has_index() ? Reg{expr.index_reg()} : Reg::make_invalid();
    ScratchReg scratch{this};
    Reg reg = scratch.alloc_gp();
    emit("expression", reg.id(), base.id(), index.id(), expr.scale, expr.disp);
    value.state = std::move(scratch);
    return reg;
  }
  void alloca_fixed(u64 size, u32 align, ValuePart &part) {
    const Reg reg = part.alloc_reg(this);
    emit("dynamic.alloca", size, align, reg.id());
  }
  void gen_func_epilog() { emit("epilogue"); }
  Jump invert_jump(Jump jump) {
    assert(jump != Jump::jmp);
    return jump == Jump::equal ? Jump::unequal : Jump::equal;
  }
  void generate_raw_jump(Jump jump, Label label) { emit("jump", u32(jump), u32(label)); }
  void switch_emit_cmpeq(Label label, Reg cmp, Reg tmp, u64 value, bool width32) {
    emit("switch.equal", u32(label), cmp.id(), tmp.id(), value, width32);
  }
  void switch_emit_binary_step(Label equal, Label greater, Reg cmp, Reg tmp, u64 value, bool width32) {
    emit("switch.binary", u32(equal), u32(greater), cmp.id(), tmp.id(), value, width32);
  }
  FunctionWriterBase::JumpTable *switch_create_jump_table(Label fallback, Reg cmp, Reg tmp,
                                                         u64 low, u64 high, bool width32) {
    emit("switch.table", u32(fallback), cmp.id(), tmp.id(), low, high, width32, jump_tables);
    if (!jump_tables) return nullptr;
    auto &table = text_writer.alloc_jump_table(u32(high - low + 1), cmp, tmp);
    table.misc = 0;
    return &table;
  }
  void snapshot(std::string_view tag) {
    record(tag, stack.frame_size, stack.has_dynamic_alloca, stack.is_leaf_function,
           stack.generated_call, stack.frame_used, register_file.allocatable,
           register_file.used, register_file.clobbered,
           this->assignments.cur_fixed_assignment_count[0], this->assignments.cur_fixed_assignment_count[1]);
    for (auto id : register_file.used_regs()) {
      const Reg reg{id};
      record("register", id, register_file.reg_local_idx(reg), register_file.reg_part(reg),
             register_file.lock_counts[id]);
    }
    for (u32 local = 0; local < this->assignments.value_ptrs.size(); ++local) {
      auto *assignment = this->assignments.value_ptrs[local];
      if (!assignment) continue;
      record("assignment", local, assignment->part_count, assignment->references_left,
             assignment->pending_free, assignment->delay_free, assignment->variable_ref,
             assignment->stack_variable, assignment->frame_off, assignment->max_part_size);
      for (u32 index = 0; index < assignment->part_count; ++index) {
        AssignmentPartRef part{assignment, index};
        record("part", local, index, part.bank().id(), part.get_reg().id(), part.part_size(),
               part.fixed_assignment(), part.register_valid(), part.modified());
      }
    }
  }

  struct CallBuilder : Base::template CallBuilderBase<CallBuilder> {
    using Parent = typename Base::template CallBuilderBase<CallBuilder>;
    using Parent::compiler;
    using Parent::assigner;
    CallBuilder(RecordingCompiler &compiler, CCAssigner &assigner) : Parent(compiler, assigner) {}
    void add_arg_stack(ValuePart &part, CCAssignment assignment) {
      Reg reg = part.cur_reg_or_load(&compiler);
      compiler.emit("call.stack", reg.id(), assignment.stack_off, assignment.size);
    }
    void add_arg_byval(ValuePart &part, CCAssignment assignment) {
      Reg reg = part.cur_reg_or_load(&compiler);
      compiler.emit("call.byval", reg.id(), assignment.stack_off, assignment.size, assignment.align);
    }
    void call_impl(std::variant<SymRef, ValuePart> &&target) {
      if (auto *symbol = std::get_if<SymRef>(&target)) compiler.emit("call.direct", symbol->id());
      else {
        auto &part = std::get<ValuePart>(target);
        compiler.emit("call.indirect", part.cur_reg_or_load(&compiler).id());
        part.reset(&compiler);
      }
    }
  };

  bool compile_inst(IRInstRef instruction, InstRange) {
    const auto &value = adaptor->ir->values[u32(instruction)];
    const auto operand = [&](u32 index) { return IRValueRef(adaptor->ir->value_operands[value.op_begin_idx + index]); };
    const auto block = [&](u32 index) { return IRBlockRef(adaptor->ir->value_operands[value.op_begin_idx + index]); };
    record("instruction", u32(instruction), u32(value.op));
    switch (value.op) {
      using enum TestIR::Value::Op;
    case alloca: break;
    case terminate: case ret: {
      RetBuilder builder{*this, *cur_cc_assigner()};
      if (value.op_count != 0) builder.add(operand(0));
      builder.ret();
      break;
    }
    case trap:
      emit("trap");
      this->release_regs_after_return();
      break;
    case br:
      this->generate_uncond_branch(block(0));
      break;
    case condbr: case tbz: {
      auto [owner, part] = this->val_ref_single(operand(0));
      const auto reg = part.load_to_reg();
      emit("condition", reg.id(), value.op == tbz ? adaptor->ir->value_operands[value.op_begin_idx + 3] : 64);
      this->generate_cond_branch(Jump::unequal, block(1), block(2));
      break;
    }
    case jump: {
      ScratchReg condition{this};
      condition.alloc_gp();
      std::vector<std::pair<u64, IRBlockRef>> cases;
      const u32 count = value.op_end_idx - value.op_begin_idx;
      assert(count > 0);
      if (switch_stride == 0 || (count > 1 && u64(count - 2) > std::numeric_limits<u64>::max() / switch_stride))
        throw std::runtime_error("invalid switch stride");
      for (u32 index = 1; index < count; ++index) cases.emplace_back((index - 1) * switch_stride, block(index));
      this->generate_switch(std::move(condition), 63, block(0), cases);
      break;
    }
    case call: {
      Assigner assigner;
      CallBuilder builder{*this, assigner};
      auto result = this->result_ref(IRValueRef(instruction));
      for (u32 index = 0; index < value.op_count; ++index) {
        CallArg argument{operand(index)};
        const auto &name = adaptor->ir->values[u32(argument.value)].name;
        if (name.starts_with("byval_")) argument = CallArg{operand(index), CallArg::Flag::byval, 8, 24};
        else if (name.starts_with("sret_")) argument = CallArg{operand(index), CallArg::Flag::sret};
        else if (name.starts_with("sext_")) { argument.flag = CallArg::Flag::sext; argument.ext_bits = 8; }
        else if (name.starts_with("zext_")) { argument.flag = CallArg::Flag::zext; argument.ext_bits = 8; }
        else if (name.starts_with("split_")) argument.flag = CallArg::Flag::allow_split;
        builder.add_arg(argument);
      }
      builder.call(this->func_syms[value.call_func_idx]);
      builder.add_ret(result);
      break;
    }
    case any: case add: case sub: case condselect: case zerofill: {
      std::vector<ValueRef> owners;
      std::vector<ValuePartRef> parts;
      owners.reserve(value.op_count);
      for (u32 index = 0; index < value.op_count; ++index) {
        owners.push_back(this->val_ref(operand(index)));
        const u32 count = val_parts(operand(index)).count();
        for (u32 part = 0; part < count; ++part) {
          parts.push_back(owners.back().part(part));
          parts.back().load_to_reg();
        }
      }
      auto result = this->result_ref(IRValueRef(instruction));
      const u32 count = val_parts(IRValueRef(instruction)).count();
      for (u32 index = 0; index < count; ++index) {
        auto part = result.part(index);
        Reg reg = Reg::make_invalid();
        if (!parts.empty() && parts[index % parts.size()].has_reg() &&
            parts[index % parts.size()].bank() == part.bank())
          reg = part.alloc_try_reuse(parts[index % parts.size()]);
        else reg = part.alloc_reg();
        emit("operation", u32(value.op), index, reg.id(), parts.size());
        part.set_modified();
      }
      break;
    }
    case none: return false;
    }
    snapshot("instruction.state");
    return true;
  }
};

template <bool Negative>
int execute(std::string_view mode, bool fixed, u64 stride, bool tables) {
  TestIR ir;
  Adaptor adaptor{&ir};
  RecordingCompiler<Negative> compiler{&adaptor};
  compiler.fixed = fixed;
  compiler.switch_stride = stride;
  compiler.jump_tables = tables;
  record("tpde.compiler.state", 1, Negative);
  if (mode == "sanitizer") {
    record("sanitizer", tpde::util::address_sanitizer_active);
    return 0;
  }
  if (mode == "compile") {
    std::string input{std::istreambuf_iterator<char>(std::cin), {}};
    if (!ir.parse_ir(input)) { record("parse", 0); return 0; }
    record("parse", 1);
    compiler.emit_bytes = true;
    adaptor.start_compile();
    bool success = compiler.compile();
    adaptor.end_compile();
    record("compile", success);
    compiler.reset();
    record("reset", compiler.func_syms.size(), compiler.block_labels.size(), compiler.personality_syms.size());
    return success ? 0 : 1;
  }
  compiler.stack.frame_size = 16;
  compiler.register_file.reset();
  compiler.register_file.allocatable = Assigner::all_regs;
  if (mode == "frame") {
    std::string command;
    while (std::cin >> command) {
      u32 size;
      if (command == "alloc") {
        if (!(std::cin >> size)) throw std::runtime_error("missing allocation size");
        i32 offset = compiler.allocate_stack_slot(size);
        record("alloc", size, offset, compiler.stack.frame_size);
      } else if (command == "free") {
        i64 offset;
        if (!(std::cin >> offset >> size)) throw std::runtime_error("missing free operands");
        compiler.free_stack_slot(u32(offset), size);
        record("free", size, offset, compiler.stack.frame_size);
      } else throw std::runtime_error("unknown frame operation");
    }
    for (u32 index = 0; index < 5; ++index) {
      record("freelist", index, compiler.stack.fixed_free_lists[index].size());
      for (auto offset : compiler.stack.fixed_free_lists[index]) record("free.slot", offset);
    }
    std::vector<u32> sizes;
    for (const auto &[size, slots] : compiler.stack.dynamic_free_lists)
      if (!slots.empty()) sizes.push_back(size);
    std::ranges::sort(sizes);
    for (auto size : sizes) {
      record("dynamic.freelist", size, compiler.stack.dynamic_free_lists[size].size());
      for (auto offset : compiler.stack.dynamic_free_lists[size]) record("free.slot", offset);
    }
    return 0;
  }
  if (mode == "guards") {
    using C = RecordingCompiler<Negative>;
    using Scratch = typename C::ScratchReg;
    using Part = typename C::ValuePartRef;
    using Generic = typename C::GenericValuePart;
    const auto registers = [&] {
      record("registers", compiler.register_file.used, compiler.register_file.clobbered);
      for (auto id : compiler.register_file.used_regs()) {
        const Reg reg{id};
        record("register", id, compiler.register_file.reg_local_idx(reg), compiler.register_file.reg_part(reg),
               compiler.register_file.lock_counts[id]);
      }
    };
    for (u32 bank = 0; bank < 2; ++bank) {
      Scratch scratch{&compiler};
      record("scratch.empty", scratch.has_reg(), scratch.release().id());
      record("scratch.alloc", scratch.alloc(RegBank{u8(bank)}).id());
      record("scratch.same", scratch.alloc(RegBank{u8(bank)}).id());
      registers();
      Scratch moved{std::move(scratch)};
      record("scratch.move", scratch.has_reg(), moved.cur_reg().id());
      record("scratch.release", moved.release().id());
      registers();
    }
    {
      Part empty{&compiler};
      record("part.default", empty.bank().id(), empty.part_size(), empty.has_assignment(), empty.is_const(), empty.has_reg());
      Part owner{&compiler, u64{19}, 8, RegBank{1}};
      owner.load_to_reg();
      auto borrowed = owner.get_unowned_ref();
      record("part.unowned", borrowed.bank().id(), borrowed.part_size(), borrowed.has_assignment(), borrowed.is_const(), borrowed.cur_reg().id());
      borrowed.reset();
      record("part.owner", owner.cur_reg().id(), compiler.register_file.is_fixed(owner.cur_reg()));
      owner.reset();
      registers();
    }
    for (u32 size : {1u, 2u, 4u, 8u}) {
      Generic value{Part{&compiler, u64{0xfedcba9876543210}, size, RegBank{0}}};
      record("immediate", value.is_imm(), value.imm_size(), value.imm64());
      Scratch destination{&compiler};
      record("generic.scratch", compiler.gval_as_reg_reuse(value, destination).id());
      record("generic.source", value.val_ref().is_const(), value.val_ref().has_reg());
      destination.reset();
      typename C::ValuePart target{RegBank{1}};
      record("generic.value", compiler.gval_as_reg_reuse(value, target).id());
      record("generic.source", value.val_ref().is_const(), value.val_ref().has_reg(), value.val_ref().bank().id());
      target.reset(&compiler);
      value.reset();
      registers();
    }
    const auto assignment = [&](Reg reg, bool fixed, u32 local = 0) {
      auto *value = compiler.assignments.allocator.allocate(1);
      value->frame_off = 0;
      value->part_count = 1;
      value->references_left = 1;
      value->max_part_size = 8;
      value->pending_free = false;
      value->variable_ref = false;
      value->stack_variable = false;
      value->delay_free = false;
      AssignmentPartRef part{value, 0};
      part.reset();
      part.set_bank(RegBank{0});
      part.set_part_size(8);
      part.set_reg(reg);
      part.set_register_valid(true);
      part.set_fixed_assignment(fixed);
      if (compiler.assignments.value_ptrs.size() <= local)
        compiler.assignments.value_ptrs.resize(local + 1, nullptr);
      compiler.assignments.value_ptrs[local] = value;
      compiler.register_file.mark_used(reg, ValLocalIdx{local}, 0);
      compiler.register_file.mark_clobbered(reg);
      if (fixed) {
        compiler.register_file.inc_lock_count(reg);
        ++compiler.assignments.cur_fixed_assignment_count[0];
      }
      return value;
    };
    {
      auto *value = assignment(Reg{2}, false);
      Part part{&compiler, ValLocalIdx{0}, value, 0, false};
      part.alloc_specific(Reg{7});
      record("specific.overwrite", part.cur_reg().id(), compiler.register_file.used,
             compiler.register_file.reg_local_idx(Reg{2}), compiler.register_file.reg_local_idx(Reg{7}));
      part.reset();
      compiler.register_file.unmark_used(Reg{2});
      compiler.release_assignment(ValLocalIdx{0}, value);
      registers();
    }
    {
      auto *source = assignment(Reg{2}, false, 0);
      auto *target = assignment(Reg{7}, false, 1);
      Part part{&compiler, ValLocalIdx{0}, source, 0, false};
      compiler.spill_scratch = true;
      part.load_to_specific(Reg{7});
      compiler.spill_scratch = false;
      record("specific.after.eviction", part.cur_reg().id(), source->frame_off,
             AssignmentPartRef{source, 0}.register_valid(), AssignmentPartRef{target, 0}.register_valid(),
             compiler.register_file.used);
      part.reset();
      compiler.release_assignment(ValLocalIdx{0}, source);
      compiler.release_assignment(ValLocalIdx{1}, target);
      registers();
    }
    {
      auto *assigned = assignment(Reg{2}, false);
      Part part{&compiler, ValLocalIdx{0}, assigned, 0, true};
      part.lock();
      Generic value{std::move(part)};
      typename C::ValuePart destination{RegBank{1}};
      record("assigned.reuse", compiler.gval_as_reg_reuse(value, destination).id());
      record("assigned.source", value.val_ref().has_assignment(), value.val_ref().local_idx(),
             value.val_ref().part(), value.val_ref().has_reg(), value.val_ref().assignment().register_valid());
      record("assigned.destination", destination.has_assignment(), destination.cur_reg().id());
      value.reset();
      destination.reset(&compiler);
      compiler.release_assignment(ValLocalIdx{0}, assigned);
      registers();
    }
    {
      auto *assigned = assignment(Reg{8}, true);
      Part destination{&compiler, ValLocalIdx{0}, assigned, 0, false};
      Scratch scratch{&compiler};
      scratch.alloc_specific(Reg{2});
      Generic value{std::move(scratch)};
      record("assigned.fixed", compiler.gval_as_reg_reuse(value, destination).id());
      record("assigned.fixed.state", destination.has_reg(), destination.assignment().get_reg().id(),
             destination.assignment().fixed_assignment(), std::get<Scratch>(value.state).has_reg());
      value.reset();
      destination.reset();
      compiler.release_assignment(ValLocalIdx{0}, assigned);
      registers();
    }
    {
      auto *assigned = assignment(Reg{8}, false);
      compiler.register_file.unmark_used(Reg{8});
      AssignmentPartRef{assigned, 0}.set_register_valid(false);
      Part destination{&compiler, ValLocalIdx{0}, assigned, 0, false};
      Scratch scratch{&compiler};
      scratch.alloc_specific(Reg{2});
      Generic value{std::move(scratch)};
      record("assigned.scratch", compiler.gval_as_reg_reuse(value, destination).id());
      record("assigned.scratch.state", destination.has_reg(), destination.cur_reg().id(),
             destination.assignment().register_valid(), compiler.register_file.is_fixed(destination.cur_reg()));
      value.reset();
      destination.reset();
      compiler.release_assignment(ValLocalIdx{0}, assigned);
      registers();
    }
    {
      Scratch base{&compiler};
      base.alloc_specific(Reg{5});
      Generic value{typename Generic::Expr{std::move(base), 0}};
      Scratch destination{&compiler};
      record("expression.trivial", compiler.gval_as_reg_reuse(value, destination).id(), destination.has_reg());
      std::get<typename Generic::Expr>(value.state).disp = 24;
      record("expression.materialized", compiler.gval_as_reg_reuse(value, destination).id());
      record("expression.owned", destination.has_reg(), std::get<Scratch>(value.state).has_reg());
      value.reset();
      destination.reset();
      registers();
    }
    return 0;
  }
  throw std::runtime_error("unknown mode");
}

int main(int argc, char **argv) {
  try {
    if (argc == 1) {
      std::string header, mode;
      if (!std::getline(std::cin, header)) throw std::runtime_error("missing request header");
      std::istringstream input{header};
      unsigned negative, fixed, tables;
      u64 stride;
      if (!(input >> mode >> negative >> fixed >> stride >> tables))
        throw std::runtime_error("expected mode negative fixed stride tables");
      std::string extra;
      if (input >> extra || negative > 1 || fixed > 1 || tables > 1)
        throw std::runtime_error("invalid request header");
      return negative ? execute<true>(mode, fixed, stride, tables)
                      : execute<false>(mode, fixed, stride, tables);
    }
    if (argc < 2 || argc > 6) throw std::runtime_error("expected mode [negative] [fixed] [stride] [tables]");
    const bool negative = argc <= 2 || std::string_view(argv[2]) != "0";
    const bool fixed = argc <= 3 || std::string_view(argv[3]) != "0";
    const u64 stride = argc <= 4 ? 1 : std::stoull(argv[4]);
    const bool tables = argc <= 5 || std::string_view(argv[5]) != "0";
    return negative ? execute<true>(argv[1], fixed, stride, tables)
                    : execute<false>(argv[1], fixed, stride, tables);
  } catch (const std::exception &error) {
    std::cerr << error.what() << '\n';
    return 2;
  }
}
