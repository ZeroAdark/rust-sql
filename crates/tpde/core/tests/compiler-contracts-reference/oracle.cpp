// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <bit>
#include <concepts>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iterator>
#include <limits>
#include <memory>
#include <optional>
#include <span>
#include <string_view>
#include <tuple>
#include <type_traits>
#include <utility>

#include "tpde/CompilerBase.hpp"
#undef ARG

namespace {
using tpde::u8;
using tpde::u32;
using tpde::u64;

class StatefulAssigner : public tpde::CCAssigner {
public:
  using CCAssigner::CCAssigner;
  u32 stack_size = 0;
  u32 next_reg[2] = {};
  u32 arg_calls = 0;
  u32 ret_calls = 0;
  u32 reset_calls = 0;

  void reset() override {
    stack_size = next_reg[0] = next_reg[1] = arg_calls = ret_calls = 0;
    ++reset_calls;
  }
  void assign_arg(tpde::CCAssignment &cca) override {
    ++arg_calls;
    const u32 bank = cca.bank.id();
    const u32 count = u32(cca.consecutive) + 1;
    if (!cca.byval && next_reg[bank] + count <= 4) {
      cca.reg = tpde::Reg{bank * 32 + next_reg[bank]};
      next_reg[bank] += count;
    } else {
      stack_size = (stack_size + cca.align - 1) & ~(u32(cca.align) - 1);
      cca.reg = tpde::Reg::make_invalid();
      cca.stack_off = stack_size;
      stack_size += cca.size;
    }
  }
  u32 get_stack_size() override { return stack_size; }
  void assign_ret(tpde::CCAssignment &cca) override {
    ++ret_calls;
    cca.reg = tpde::Reg{u32(cca.bank.id()) * 32 + (cca.sret ? 3u : 0u)};
    cca.stack_off = 0;
  }
};
class VarargAssigner : public StatefulAssigner {
public:
  using StatefulAssigner::StatefulAssigner;
  bool is_vararg() const override { return true; }
};
struct PrivateAssigner : private StatefulAssigner {};
struct ProtectedAssigner : protected StatefulAssigner {};
struct LeftAssigner : StatefulAssigner {};
struct RightAssigner : StatefulAssigner {};
struct AmbiguousAssigner : LeftAssigner, RightAssigner {};
struct UnrelatedAssigner {};

template <u32 Banks = 2, bool DefaultVariables = true>
struct Config : tpde::CompilerConfigDefault {
  static constexpr bool FRAME_INDEXING_NEGATIVE = true;
  static constexpr u32 PLATFORM_POINTER_SIZE = 8;
  static constexpr u32 NUM_BANKS = Banks;
  static constexpr bool DEFAULT_VAR_REF_HANDLING = DefaultVariables;
  using DefaultCCAssigner = StatefulAssigner;
  using Assembler = tpde::Assembler;
  using AsmReg = tpde::Reg;
  using FunctionWriter = tpde::FunctionWriterBase;
};
using GoodConfig = Config<>;
#define HIDE_CONFIG(Name, Member) \
  struct Name : GoodConfig { private: using GoodConfig::Member; }
HIDE_CONFIG(NoFrameDirection, FRAME_INDEXING_NEGATIVE);
HIDE_CONFIG(NoPointerSize, PLATFORM_POINTER_SIZE);
HIDE_CONFIG(NoBankCount, NUM_BANKS);
HIDE_CONFIG(NoVariablePolicy, DEFAULT_VAR_REF_HANDLING);
HIDE_CONFIG(NoAssignerType, DefaultCCAssigner);
HIDE_CONFIG(NoAssemblerType, Assembler);
HIDE_CONFIG(NoAsmRegType, AsmReg);
HIDE_CONFIG(NoWriterType, FunctionWriter);
#undef HIDE_CONFIG
struct WrongFrameType : GoodConfig {
  static constexpr u32 FRAME_INDEXING_NEGATIVE = 1;
};
struct WrongPointerType : GoodConfig {
  static constexpr u64 PLATFORM_POINTER_SIZE = 8;
};
struct WrongBankType : GoodConfig {
  static constexpr int NUM_BANKS = 2;
};
struct WrongVariableType : GoodConfig {
  static constexpr u8 DEFAULT_VAR_REF_HANDLING = 1;
};
struct VolatilePointer : GoodConfig {
  static volatile u32 PLATFORM_POINTER_SIZE;
};
struct ZeroPointer : GoodConfig {
  static constexpr u32 PLATFORM_POINTER_SIZE = 0;
};
struct MutableConfig : GoodConfig {
  static bool FRAME_INDEXING_NEGATIVE;
  static u32 PLATFORM_POINTER_SIZE;
  static u32 NUM_BANKS;
  static bool DEFAULT_VAR_REF_HANDLING;
};
struct VoidAssociatedTypes : GoodConfig {
  using Assembler = void;
  using AsmReg = void;
  using FunctionWriter = void;
};
template <typename Assigner>
struct ConfigAssigner : GoodConfig { using DefaultCCAssigner = Assigner; };

struct Parts {
  u32 count();
  u32 size_bytes(u32);
  tpde::RegBank reg_bank(u32);
};
struct ImplicitBool { operator bool() const; };
struct ExplicitBool { explicit operator bool() const; };
struct ImplicitWord { operator u32() const; };
struct ExplicitWord { explicit operator u32() const; };
struct ImplicitBank { operator tpde::RegBank() const; };
struct ExplicitBank { explicit operator tpde::RegBank() const; };
struct ConvertibleParts {
  ImplicitWord count();
  double size_bytes(u32);
  ImplicitBank reg_bank(u32);
};
struct ReferencedParts {
  const u32 &count() &;
  u32 &size_bytes(const u32 &) &;
  const tpde::RegBank &reg_bank(u32) &;
};
struct NonPowerOfTwoParts {
  u32 count() { return 1; }
  u32 size_bytes(u32) { return 3; }
  tpde::RegBank reg_bank(u32) { return tpde::RegBank{0}; }
};
struct NoPartCount : Parts { void count() = delete; };
struct NoPartSize : Parts { void size_bytes(u32) = delete; };
struct NoPartBank : Parts { void reg_bank(u32) = delete; };
struct VoidPartCount : Parts { void count(); };
struct VoidPartSize : Parts { void size_bytes(u32); };
struct IntegerPartBank : Parts { u32 reg_bank(u32); };
struct ExplicitPartCount : Parts { ExplicitWord count(); };
struct ExplicitPartSize : Parts { ExplicitWord size_bytes(u32); };
struct ExplicitPartBank : Parts { ExplicitBank reg_bank(u32); };
struct RvalueParts : Parts { u32 count() &&; };
struct LvalueSizeArgument : Parts { u32 size_bytes(u32 &); };
struct LvalueBankArgument : Parts { tpde::RegBank reg_bank(u32 &); };

struct Special { u8 mode = 4; u64 payload = 0; };
struct ZeroMode { u8 mode = 0; };
struct DroppySpecial { u8 mode; ~DroppySpecial() {} };
union UnionSpecial { u8 mode; u64 payload; };
struct NoMode { u8 value; };
struct ConstMode { const u8 mode = 4; };
struct VolatileMode { volatile u8 mode; };
struct WideMode { uint16_t mode; };
struct SignedMode { int8_t mode; };
struct ArrayMode { u8 mode[1]; };
struct BitfieldMode { u8 mode : 4; };
struct OffsetMode { u8 prefix; u8 mode; };
struct NonstandardMode { virtual ~NonstandardMode() {} u8 mode; };
struct PrivateMode { private: u8 mode; };
struct GenericPart {};
struct ResultPart {};
struct ConvertibleReg { operator tpde::Reg() const; };
struct ConvertibleSym { operator tpde::SymRef() const; };
struct ConvertibleResultPart { operator ResultPart() const; };
struct ConvertibleOptional { operator std::optional<Special>() const; };
struct ConvertibleAssigner { operator tpde::CCAssigner *() const; };

template <typename Cfg = GoodConfig>
struct CompilerFixture {
  using IRValueRef = u32;
  using IRInstRef = u32;
  using IRFuncRef = u32;
  using InstRange = std::span<const u32>;
  using GenericValuePart = GenericPart;
  using ValRefSpecial = Special;
  using ValuePart = ResultPart;
  static constexpr u32 NUM_FIXED_ASSIGNMENTS[Cfg::NUM_BANKS] = {};
  void start_func(u32);
  void prologue_assign_arg(tpde::CCAssigner *, u32, IRValueRef);
  void finish_func(u32);
  void spill_reg(tpde::Reg, u32, u32);
  void load_from_stack(tpde::Reg, u32, u32);
  void mov(tpde::Reg, tpde::Reg, u32);
  tpde::Reg gval_expr_as_reg(GenericPart &);
  tpde::Reg select_fixed_assignment_reg(tpde::AssignmentPartRef, IRValueRef);
  bool cur_func_may_emit_calls();
  tpde::SymRef cur_personality_func();
  tpde::CCAssigner *cur_cc_assigner();
  bool try_force_fixed_assignment(IRValueRef);
  Parts val_parts(IRValueRef);
  std::optional<Special> val_ref_special(IRValueRef);
  ResultPart val_part_ref_special(Special &, u32);
  void define_func_idx(IRFuncRef, u32);
  bool compile_inst(IRInstRef, InstRange);
};
using GoodCompiler = CompilerFixture<>;
#define HIDE_COMPILER(Name, Member) \
  struct Name : GoodCompiler { private: using GoodCompiler::Member; }
HIDE_COMPILER(NoFixedArray, NUM_FIXED_ASSIGNMENTS);
HIDE_COMPILER(NoStart, start_func);
HIDE_COMPILER(NoPrologue, prologue_assign_arg);
HIDE_COMPILER(NoFinish, finish_func);
HIDE_COMPILER(NoSpill, spill_reg);
HIDE_COMPILER(NoLoad, load_from_stack);
HIDE_COMPILER(NoMove, mov);
HIDE_COMPILER(NoGenericReg, gval_expr_as_reg);
HIDE_COMPILER(NoFixedReg, select_fixed_assignment_reg);
HIDE_COMPILER(NoCalls, cur_func_may_emit_calls);
HIDE_COMPILER(NoPersonality, cur_personality_func);
HIDE_COMPILER(NoCurrentAssigner, cur_cc_assigner);
HIDE_COMPILER(NoTryForce, try_force_fixed_assignment);
HIDE_COMPILER(NoValueParts, val_parts);
HIDE_COMPILER(NoSpecialType, ValRefSpecial);
HIDE_COMPILER(NoSpecialValue, val_ref_special);
HIDE_COMPILER(NoSpecialPart, val_part_ref_special);
HIDE_COMPILER(NoDefine, define_func_idx);
HIDE_COMPILER(NoInstruction, compile_inst);
HIDE_COMPILER(NoValueType, IRValueRef);
HIDE_COMPILER(NoInstType, IRInstRef);
HIDE_COMPILER(NoFuncType, IRFuncRef);
HIDE_COMPILER(NoRangeType, InstRange);
HIDE_COMPILER(NoGenericType, GenericValuePart);
HIDE_COMPILER(NoPartType, ValuePart);
#undef HIDE_COMPILER
struct ShortFixedArray : GoodCompiler { static constexpr u32 NUM_FIXED_ASSIGNMENTS[1] = {}; };
struct LongFixedArray : GoodCompiler { static constexpr u32 NUM_FIXED_ASSIGNMENTS[3] = {}; };
struct WideFixedArray : GoodCompiler { static constexpr u64 NUM_FIXED_ASSIGNMENTS[2] = {}; };
struct SignedFixedArray : GoodCompiler { static constexpr int NUM_FIXED_ASSIGNMENTS[2] = {}; };
struct PointerFixedArray : GoodCompiler { static constexpr const u32 *NUM_FIXED_ASSIGNMENTS = nullptr; };
struct StdFixedArray : GoodCompiler { static constexpr std::array<u32, 2> NUM_FIXED_ASSIGNMENTS = {}; };
struct VolatileFixedArray : GoodCompiler { static volatile u32 NUM_FIXED_ASSIGNMENTS[2]; };
struct MutableFixedArray : GoodCompiler { static u32 NUM_FIXED_ASSIGNMENTS[2]; };
struct GenericRegReference : GoodCompiler { tpde::Reg &gval_expr_as_reg(GenericPart &); };
struct GenericRegConversion : GoodCompiler { ConvertibleReg gval_expr_as_reg(GenericPart &); };
struct FixedRegReference : GoodCompiler { tpde::Reg &select_fixed_assignment_reg(tpde::AssignmentPartRef, u32); };
struct FixedRegConversion : GoodCompiler { ConvertibleReg select_fixed_assignment_reg(tpde::AssignmentPartRef, u32); };
struct PersonalityReference : GoodCompiler { tpde::SymRef &cur_personality_func(); };
struct PersonalityConversion : GoodCompiler { ConvertibleSym cur_personality_func(); };
struct SpecialReference : GoodCompiler { std::optional<Special> &val_ref_special(u32); };
struct SpecialConversion : GoodCompiler { ConvertibleOptional val_ref_special(u32); };
struct BareSpecial : GoodCompiler { Special val_ref_special(u32); };
struct DifferentOptional : GoodCompiler { std::optional<ZeroMode> val_ref_special(u32); };
struct PartReference : GoodCompiler { ResultPart &val_part_ref_special(Special &, u32); };
struct PartConversion : GoodCompiler { ConvertibleResultPart val_part_ref_special(Special &, u32); };
struct BadSpecialType : GoodCompiler { using ValRefSpecial = OffsetMode; };
struct BadValueParts : GoodCompiler { VoidPartSize val_parts(u32); };
struct ConvertibleReturns : GoodCompiler {
  ImplicitBool cur_func_may_emit_calls();
  StatefulAssigner *cur_cc_assigner();
  int try_force_fixed_assignment(u32);
  ImplicitBool compile_inst(u32, InstRange);
  ConvertibleParts val_parts(u32);
};
struct ProxyAssignerReturn : GoodCompiler { ConvertibleAssigner cur_cc_assigner(); };
struct ExplicitCalls : GoodCompiler { ExplicitBool cur_func_may_emit_calls(); };
struct ExplicitTryForce : GoodCompiler { ExplicitBool try_force_fixed_assignment(u32); };
struct ExplicitInstruction : GoodCompiler { ExplicitBool compile_inst(u32, InstRange); };
struct ConstAssignerReturn : GoodCompiler { const tpde::CCAssigner *cur_cc_assigner(); };
struct NonPointerAssigner : GoodCompiler { tpde::CCAssigner &cur_cc_assigner(); };
struct UncheckedReturns : GoodCompiler {
  u64 start_func(u32);
  int prologue_assign_arg(tpde::CCAssigner *, u32, u32);
  ImplicitBool finish_func(u32);
  std::nullptr_t spill_reg(tpde::Reg, u32, u32);
  float load_from_stack(tpde::Reg, u32, u32);
  Special mov(tpde::Reg, tpde::Reg, u32);
  ResultPart define_func_idx(u32, u32);
};
struct RvalueStart : GoodCompiler { void start_func(u32) &&; };
struct LvalueStartArgument : GoodCompiler { void start_func(u32 &); };
struct WrongPrologueArgument : GoodCompiler { void prologue_assign_arg(u32, u32, u32); };
struct RvalueGenericPart : GoodCompiler { tpde::Reg gval_expr_as_reg(GenericPart &&); };
struct RvalueSpecialPart : GoodCompiler { ResultPart val_part_ref_special(Special &&, u32); };
struct CustomCompiler : CompilerFixture<Config<2, false>> {
  void setup_var_ref_assignments();
  void load_address_of_var_reference(tpde::Reg, tpde::AssignmentPartRef);
};
struct NoCustomSetup : CustomCompiler {
private:
  using CustomCompiler::setup_var_ref_assignments;
};
struct NoCustomLoad : CustomCompiler {
private:
  using CustomCompiler::load_address_of_var_reference;
};
struct WrongCustomLoad : CustomCompiler { void load_address_of_var_reference(u32, u32); };
struct UncheckedCustomReturns : CustomCompiler {
  int setup_var_ref_assignments();
  ResultPart load_address_of_var_reference(tpde::Reg, tpde::AssignmentPartRef);
};
struct DeletedCustomHooks : GoodCompiler {
  void setup_var_ref_assignments() = delete;
  void load_address_of_var_reference(tpde::Reg, tpde::AssignmentPartRef) = delete;
};

#define POS(...) [] { static_assert((__VA_ARGS__), #__VA_ARGS__); return true; }()
#define NEG(...) [] { static_assert(!(__VA_ARGS__), #__VA_ARGS__); return true; }()
constexpr bool positive_checks[] = {
  POS(tpde::SameBaseAs<u32, u32>),
  POS(tpde::SameBaseAs<const u32 &, u32>),
  POS(tpde::SameBaseAs<u32 &&, u32>),
  POS(tpde::SameBaseAs<const u32 (&)[2], u32[2]>),
  POS(tpde::CompilerConfig<GoodConfig>),
  POS(tpde::CompilerConfig<Config<1>>),
  POS(tpde::CompilerConfig<Config<8>>),
  POS(tpde::CompilerConfig<Config<0>>),
  POS(tpde::CompilerConfig<Config<2, false>>),
  POS(tpde::CompilerConfig<ZeroPointer>),
  POS(tpde::CompilerConfig<MutableConfig>),
  POS(tpde::CompilerConfig<VoidAssociatedTypes>),
  POS(tpde::CompilerConfig<ConfigAssigner<tpde::CCAssigner>>),
  POS(tpde::CompilerConfig<ConfigAssigner<VarargAssigner>>),
  POS(tpde::CompilerConfigDefault::DEFAULT_VAR_REF_HANDLING),
  POS(tpde::IsTrue<true>),
  POS(tpde::ValueParts<Parts>),
  POS(tpde::ValueParts<ConvertibleParts>),
  POS(tpde::ValueParts<ReferencedParts>),
  POS(tpde::ValueParts<NonPowerOfTwoParts>),
  POS(tpde::ValRefSpecialStruct<Special>),
  POS(tpde::ValRefSpecialStruct<ZeroMode>),
  POS(tpde::ValRefSpecialStruct<DroppySpecial>),
  POS(tpde::ValRefSpecialStruct<UnionSpecial>),
  POS(tpde::Compiler<GoodCompiler, GoodConfig>),
  POS(tpde::Compiler<CompilerFixture<Config<1>>, Config<1>>),
  POS(tpde::Compiler<CompilerFixture<Config<8>>, Config<8>>),
  POS(tpde::Compiler<MutableFixedArray, GoodConfig>),
  POS(tpde::Compiler<ConvertibleReturns, GoodConfig>),
  POS(tpde::Compiler<ProxyAssignerReturn, GoodConfig>),
  POS(tpde::Compiler<UncheckedReturns, GoodConfig>),
  POS(tpde::Compiler<CustomCompiler, Config<2, false>>),
  POS(tpde::Compiler<UncheckedCustomReturns, Config<2, false>>),
  POS(tpde::Compiler<DeletedCustomHooks, GoodConfig>),
  POS(std::is_abstract_v<tpde::CCAssigner>),
  POS(std::has_virtual_destructor_v<tpde::CCAssigner>),
  POS(std::is_constructible_v<StatefulAssigner, const tpde::CCInfo &>),
  POS(std::is_same_v<decltype(std::declval<const tpde::CCAssigner &>().get_ccinfo()), const tpde::CCInfo &>),
  POS(std::is_same_v<decltype(tpde::CCAssigner::ccinfo), const tpde::CCInfo *>),
  POS(std::is_same_v<decltype(tpde::CCInfo::allocatable_regs), const u64>),
  POS(std::is_same_v<decltype(tpde::CCInfo::callee_saved_regs), const u64>),
  POS(std::is_same_v<decltype(tpde::CCInfo::arg_regs), const u64>),
  POS(std::is_same_v<decltype(tpde::CCInfo::red_zone_size), const u8>),
};
constexpr bool negative_checks[] = {
  NEG(tpde::SameBaseAs<volatile u32 &, u32>),
  NEG(tpde::SameBaseAs<const volatile u32 &, u32>),
  NEG(tpde::SameBaseAs<u32, const u32>),
  NEG(tpde::SameBaseAs<u64, u32>),
  NEG(tpde::SameBaseAs<const u32 (&)[1], u32[2]>),
  NEG(tpde::SameBaseAs<const u32 *, u32[2]>),
  NEG(tpde::SameBaseAs<std::array<u32, 2>, u32[2]>),
  NEG(tpde::CompilerConfig<tpde::CompilerConfigDefault>),
  NEG(tpde::CompilerConfig<NoFrameDirection>),
  NEG(tpde::CompilerConfig<NoPointerSize>),
  NEG(tpde::CompilerConfig<NoBankCount>),
  NEG(tpde::CompilerConfig<NoVariablePolicy>),
  NEG(tpde::CompilerConfig<NoAssignerType>),
  NEG(tpde::CompilerConfig<NoAssemblerType>),
  NEG(tpde::CompilerConfig<NoAsmRegType>),
  NEG(tpde::CompilerConfig<NoWriterType>),
  NEG(tpde::CompilerConfig<WrongFrameType>),
  NEG(tpde::CompilerConfig<WrongPointerType>),
  NEG(tpde::CompilerConfig<WrongBankType>),
  NEG(tpde::CompilerConfig<WrongVariableType>),
  NEG(tpde::CompilerConfig<VolatilePointer>),
  NEG(tpde::CompilerConfig<ConfigAssigner<PrivateAssigner>>),
  NEG(tpde::CompilerConfig<ConfigAssigner<ProtectedAssigner>>),
  NEG(tpde::CompilerConfig<ConfigAssigner<AmbiguousAssigner>>),
  NEG(tpde::CompilerConfig<ConfigAssigner<UnrelatedAssigner>>),
  NEG(tpde::CompilerConfig<ConfigAssigner<tpde::CCAssigner *>>),
  NEG(tpde::IsTrue<false>),
  NEG(tpde::ValueParts<NoPartCount>),
  NEG(tpde::ValueParts<NoPartSize>),
  NEG(tpde::ValueParts<NoPartBank>),
  NEG(tpde::ValueParts<VoidPartCount>),
  NEG(tpde::ValueParts<VoidPartSize>),
  NEG(tpde::ValueParts<IntegerPartBank>),
  NEG(tpde::ValueParts<ExplicitPartCount>),
  NEG(tpde::ValueParts<ExplicitPartSize>),
  NEG(tpde::ValueParts<ExplicitPartBank>),
  NEG(tpde::ValueParts<RvalueParts>),
  NEG(tpde::ValueParts<LvalueSizeArgument>),
  NEG(tpde::ValueParts<LvalueBankArgument>),
  NEG(tpde::ValRefSpecialStruct<NoMode>),
  NEG(tpde::ValRefSpecialStruct<ConstMode>),
  NEG(tpde::ValRefSpecialStruct<VolatileMode>),
  NEG(tpde::ValRefSpecialStruct<WideMode>),
  NEG(tpde::ValRefSpecialStruct<SignedMode>),
  NEG(tpde::ValRefSpecialStruct<ArrayMode>),
  NEG(tpde::ValRefSpecialStruct<BitfieldMode>),
  NEG(tpde::ValRefSpecialStruct<OffsetMode>),
  NEG(tpde::ValRefSpecialStruct<NonstandardMode>),
  NEG(tpde::ValRefSpecialStruct<PrivateMode>),
  NEG(tpde::Compiler<GoodCompiler, WrongFrameType>),
  NEG(tpde::Compiler<GoodCompiler, Config<0>>),
  NEG(tpde::Compiler<NoFixedArray, GoodConfig>),
  NEG(tpde::Compiler<NoStart, GoodConfig>),
  NEG(tpde::Compiler<NoPrologue, GoodConfig>),
  NEG(tpde::Compiler<NoFinish, GoodConfig>),
  NEG(tpde::Compiler<NoSpill, GoodConfig>),
  NEG(tpde::Compiler<NoLoad, GoodConfig>),
  NEG(tpde::Compiler<NoMove, GoodConfig>),
  NEG(tpde::Compiler<NoGenericReg, GoodConfig>),
  NEG(tpde::Compiler<NoFixedReg, GoodConfig>),
  NEG(tpde::Compiler<NoCalls, GoodConfig>),
  NEG(tpde::Compiler<NoPersonality, GoodConfig>),
  NEG(tpde::Compiler<NoCurrentAssigner, GoodConfig>),
  NEG(tpde::Compiler<NoTryForce, GoodConfig>),
  NEG(tpde::Compiler<NoValueParts, GoodConfig>),
  NEG(tpde::Compiler<NoSpecialType, GoodConfig>),
  NEG(tpde::Compiler<NoSpecialValue, GoodConfig>),
  NEG(tpde::Compiler<NoSpecialPart, GoodConfig>),
  NEG(tpde::Compiler<NoDefine, GoodConfig>),
  NEG(tpde::Compiler<NoInstruction, GoodConfig>),
  NEG(tpde::Compiler<NoValueType, GoodConfig>),
  NEG(tpde::Compiler<NoInstType, GoodConfig>),
  NEG(tpde::Compiler<NoFuncType, GoodConfig>),
  NEG(tpde::Compiler<NoRangeType, GoodConfig>),
  NEG(tpde::Compiler<NoGenericType, GoodConfig>),
  NEG(tpde::Compiler<NoPartType, GoodConfig>),
  NEG(tpde::Compiler<ShortFixedArray, GoodConfig>),
  NEG(tpde::Compiler<LongFixedArray, GoodConfig>),
  NEG(tpde::Compiler<WideFixedArray, GoodConfig>),
  NEG(tpde::Compiler<SignedFixedArray, GoodConfig>),
  NEG(tpde::Compiler<PointerFixedArray, GoodConfig>),
  NEG(tpde::Compiler<StdFixedArray, GoodConfig>),
  NEG(tpde::Compiler<VolatileFixedArray, GoodConfig>),
  NEG(tpde::Compiler<GenericRegReference, GoodConfig>),
  NEG(tpde::Compiler<GenericRegConversion, GoodConfig>),
  NEG(tpde::Compiler<FixedRegReference, GoodConfig>),
  NEG(tpde::Compiler<FixedRegConversion, GoodConfig>),
  NEG(tpde::Compiler<PersonalityReference, GoodConfig>),
  NEG(tpde::Compiler<PersonalityConversion, GoodConfig>),
  NEG(tpde::Compiler<SpecialReference, GoodConfig>),
  NEG(tpde::Compiler<SpecialConversion, GoodConfig>),
  NEG(tpde::Compiler<BareSpecial, GoodConfig>),
  NEG(tpde::Compiler<DifferentOptional, GoodConfig>),
  NEG(tpde::Compiler<PartReference, GoodConfig>),
  NEG(tpde::Compiler<PartConversion, GoodConfig>),
  NEG(tpde::Compiler<BadSpecialType, GoodConfig>),
  NEG(tpde::Compiler<BadValueParts, GoodConfig>),
  NEG(tpde::Compiler<ExplicitCalls, GoodConfig>),
  NEG(tpde::Compiler<ExplicitTryForce, GoodConfig>),
  NEG(tpde::Compiler<ExplicitInstruction, GoodConfig>),
  NEG(tpde::Compiler<ConstAssignerReturn, GoodConfig>),
  NEG(tpde::Compiler<NonPointerAssigner, GoodConfig>),
  NEG(tpde::Compiler<RvalueStart, GoodConfig>),
  NEG(tpde::Compiler<LvalueStartArgument, GoodConfig>),
  NEG(tpde::Compiler<WrongPrologueArgument, GoodConfig>),
  NEG(tpde::Compiler<RvalueGenericPart, GoodConfig>),
  NEG(tpde::Compiler<RvalueSpecialPart, GoodConfig>),
  NEG(tpde::Compiler<CompilerFixture<Config<2, false>>, Config<2, false>>),
  NEG(tpde::Compiler<NoCustomSetup, Config<2, false>>),
  NEG(tpde::Compiler<NoCustomLoad, Config<2, false>>),
  NEG(tpde::Compiler<WrongCustomLoad, Config<2, false>>),
  NEG(std::is_default_constructible_v<StatefulAssigner>),
};
#undef POS
#undef NEG

void word(u32 value) {
  const unsigned char bytes[] = {
      static_cast<unsigned char>(value), static_cast<unsigned char>(value >> 8),
      static_cast<unsigned char>(value >> 16), static_cast<unsigned char>(value >> 24)};
  if (std::fwrite(bytes, 1, sizeof(bytes), stdout) != sizeof(bytes)) std::abort();
}
void wide_word(u64 value) {
  word(static_cast<u32>(value));
  word(static_cast<u32>(value >> 32));
}
void assignment(const tpde::CCAssignment &cca) {
  word(cca.reg.id());
  word(cca.consecutive);
  word(cca.sret);
  word(cca.byval);
  word(cca.int_ext);
  word(cca.align);
  word(cca.bank.id());
  word(cca.size);
  word(cca.stack_off);
}
void fields() {
  assignment(tpde::CCAssignment{});
  constexpr u32 sizes[] = {0, 1, 7, 8, 255, 65536, 0x80000000, 0xffffffff};
  for (u32 v = 0; v < 256; ++v) {
    for (u32 flags = 0; flags < 4; ++flags) {
      for (u32 i = 0; i < std::size(sizes); ++i) {
        tpde::CCAssignment cca;
        cca.reg = tpde::Reg{v};
        cca.consecutive = static_cast<u8>(v);
        cca.sret = flags & 1;
        cca.byval = (flags >> 1) & 1;
        cca.int_ext = static_cast<u8>(v ^ 0x80);
        cca.align = static_cast<u8>(255 - v);
        cca.bank = tpde::RegBank{static_cast<u8>(v)};
        cca.size = sizes[i];
        cca.stack_off = sizes[7 - i];
        assignment(cca);
      }
    }
  }
}
void layout() {
  for (const std::size_t value : {
      sizeof(tpde::CCAssignment), alignof(tpde::CCAssignment),
      offsetof(tpde::CCAssignment, reg), offsetof(tpde::CCAssignment, consecutive),
      offsetof(tpde::CCAssignment, int_ext), offsetof(tpde::CCAssignment, align),
      offsetof(tpde::CCAssignment, bank), offsetof(tpde::CCAssignment, size),
      offsetof(tpde::CCAssignment, stack_off), sizeof(tpde::CCInfo),
      alignof(tpde::CCInfo), sizeof(Special), alignof(Special),
      offsetof(Special, mode), offsetof(Special, payload)}) {
    word(static_cast<u32>(value));
  }
}
void info(tpde::CCAssigner &assigner, const tpde::CCInfo &expected) {
  const auto &value = assigner.get_ccinfo();
  wide_word(value.allocatable_regs);
  wide_word(value.callee_saved_regs);
  wide_word(value.arg_regs);
  word(value.red_zone_size);
  word(assigner.ccinfo == &expected);
  word(&value == &expected);
  word(assigner.is_vararg());
}
void state(tpde::CCAssigner &base, const StatefulAssigner &fixture) {
  word(base.get_stack_size());
  word(fixture.next_reg[0]);
  word(fixture.next_reg[1]);
  word(fixture.arg_calls);
  word(fixture.ret_calls);
  word(fixture.reset_calls);
}
void assigner() {
  const tpde::CCInfo original{0xfedcba9876543210, 0x8000000000000001, 0x000000030000000c};
  const tpde::CCInfo alternative{0xffffffffffffffff, 0, 0xaaaaaaaa55555555, 128};
  StatefulAssigner fixture(original);
  VarargAssigner vararg(original);
  tpde::CCAssigner &base = fixture;
  info(base, original);
  base.ccinfo = &alternative;
  info(base, alternative);
  info(vararg, original);
  for (u32 epoch = 0; epoch < 3; ++epoch) {
    base.reset();
    word(epoch);
    state(base, fixture);
    for (u32 j = 0; j < 12; ++j) {
      tpde::CCAssignment cca;
      cca.consecutive = static_cast<u8>(j % 3);
      cca.sret = j == 0;
      cca.byval = j % 5 == 0;
      cca.int_ext = static_cast<u8>(((j & 1) ? 0x80 : 0) | (8 << (j % 4)));
      cca.align = static_cast<u8>(1 << (j % 5));
      cca.bank = tpde::RegBank{static_cast<u8>(j % 2)};
      cca.size = 1 << (j % 7);
      base.assign_arg(cca);
      assignment(cca);
      state(base, fixture);
    }
    tpde::CCAssignment cca;
    cca.consecutive = 2;
    cca.sret = epoch & 1;
    cca.int_ext = 0x88;
    cca.align = 16;
    cca.bank = tpde::RegBank{static_cast<u8>(epoch % 2)};
    cca.size = 16;
    cca.stack_off = 0xffffffff;
    base.assign_ret(cca);
    assignment(cca);
    state(base, fixture);
  }
}
}

int main(int argc, char **argv) {
  char input[32] = {};
  const char *mode;
  if (argc == 2) mode = argv[1];
  else if (argc == 1 && std::scanf("%31s", input) == 1) mode = input;
  else return 2;
  if (std::strcmp(mode, "fields") == 0) fields();
  else if (std::strcmp(mode, "assigner") == 0) assigner();
  else if (std::strcmp(mode, "layout") == 0) layout();
  else if (std::strcmp(mode, "concepts") == 0) {
    word(1);
    word(std::size(positive_checks));
    word(std::size(negative_checks));
  } else return 2;
  return std::fflush(stdout) == 0 ? 0 : 1;
}
