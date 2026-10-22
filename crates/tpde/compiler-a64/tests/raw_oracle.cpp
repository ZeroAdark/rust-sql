// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <cstdint>
#include <iostream>
#include <optional>
#include <stdexcept>
#include <vector>
#include "tpde/arm64/CompilerA64.hpp"
#include "tpde/src/test/TestIRAdaptor.hpp"

using namespace tpde;
using tpde::a64::AsmReg;
static uint64_t number() {
  uint64_t result=0;
  for(unsigned i=0;i<8;++i) {
    int byte=std::cin.get();
    if(byte==EOF) throw std::runtime_error("truncated raw operation");
    result|=uint64_t(uint8_t(byte))<<(8*i);
  }
  return result;
}
static void put(uint64_t value) {
  for(unsigned i=0;i<8;++i) std::cout.put(char(value>>(8*i)));
}
struct Operation { uint64_t code; std::array<uint64_t,8> args; };

struct RawCompiler : a64::CompilerA64<test::TestIRAdaptor,RawCompiler> {
  using Base=a64::CompilerA64<test::TestIRAdaptor,RawCompiler>;
  const std::vector<Operation> &operations;
  RawCompiler(test::TestIRAdaptor *adaptor,const std::vector<Operation> &ops):Base(adaptor),operations(ops) {}
  struct Parts {
    static u32 count() {return 1;}
    static u32 size_bytes(u32) {return 8;}
    static RegBank reg_bank(u32) {return RegBank{0};}
  };
  Parts val_parts(IRValueRef) {return {};}
  SymRef cur_personality_func() const {return {};}
  bool cur_func_may_emit_calls() const {return false;}
  std::optional<ValRefSpecial> val_ref_special(IRValueRef) {return {};}
  ValuePart val_part_ref_special(ValRefSpecial &,u32) {std::abort();}
  void define_func_idx(IRFuncRef function,u32 index) {assert(u32(function)==index);}
  bool try_force_fixed_assignment(IRValueRef) const {return false;}
  bool compile_inst(IRInstRef,InstRange) {
    std::vector<Label> labels;
    for(const auto &operation:operations) {
      const auto &a=operation.args;
      switch(operation.code) {
      case 1: load_off(AsmReg{a[0]},AsmReg{a[1]},a[2],a[3],a[4]);break;
      case 2: store_off(AsmReg{a[0]},a[1],AsmReg{a[2]},a[3]);break;
      case 3: mov(AsmReg{a[0]},AsmReg{a[1]},a[2]);break;
      case 4: {
        const u64 words[2]{a[3],a[4]};
        materialize_constant(words,RegBank{u8(a[0])},a[1],AsmReg{a[2]});break;
      }
      case 5: generate_raw_set(Jump{Jump::Kind(a[0])},AsmReg{a[1]});break;
      case 6: generate_raw_mask(Jump{Jump::Kind(a[0])},AsmReg{a[1]});break;
      case 7: generate_raw_select(Jump{Jump::Kind(a[0])},AsmReg{a[1]},AsmReg{a[2]},AsmReg{a[3]},a[4]);break;
      case 8: generate_raw_intext(AsmReg{a[0]},AsmReg{a[1]},a[2],a[3],a[4]);break;
      case 9: generate_raw_bfi(AsmReg{a[0]},AsmReg{a[1]},a[2],a[3]);break;
      case 10: generate_raw_bfiz(AsmReg{a[0]},AsmReg{a[1]},a[2],a[3]);break;
      case 11: {
        const auto kind=Jump::Kind(a[0]);
        const Jump jump=a[0]<15?Jump{kind}:a[0]<17?Jump{kind,AsmReg{a[1]},bool(a[2])}:Jump{kind,AsmReg{a[1]},u8(a[3])};
        generate_raw_jump(jump,labels.at(a[4]));break;
      }
      case 12: labels.push_back(text_writer.label_create());break;
      case 13: text_writer.label_place(labels.at(a[0]),text_writer.offset());break;
      case 14: {
        const u64 end=text_writer.offset()+a[0];
        while(text_writer.offset()<end) text_writer.write_inst(0xd503201f);
        break;
      }
      case 15: switch_emit_cmpeq(labels.at(a[0]),AsmReg{a[1]},AsmReg{a[2]},a[3],a[4]);break;
      case 16: switch_emit_binary_step(labels.at(a[0]),labels.at(a[1]),AsmReg{a[2]},AsmReg{a[3]},a[4],a[5]);break;
      case 17: {
        auto *table=switch_create_jump_table(labels.at(a[0]),AsmReg{a[1]},AsmReg{a[2]},a[3],a[4],a[5]);
        for(u32 i=0;i<table->size;++i) table->labels()[i]=labels.at(a[6+i%2]);
        break;
      }
      case 18:
        cpu_feats=CPU_FEATURES(a[0]);put(has_cpu_feats(CPU_FEATURES(a[1])));break;
      case 19: {
        const Jump jump{Jump::Kind(a[0])};
        put(a[1]?swap_jump(jump).kind:invert_jump(jump).kind);break;
      }
      case 20: {
        const auto kind=Jump::Kind(a[0]);
        const Jump jump=a[0]<17?Jump{kind,AsmReg{a[1]},bool(a[2])}:Jump{kind,AsmReg{a[1]},u8(a[2])};
        const auto inverted=invert_jump(jump);
        put(inverted.kind);put(inverted.cmp_reg.id());put(inverted.cmp_is_32);put(inverted.test_bit);break;
      }
      default: throw std::runtime_error("unknown raw operation");
      }
      put(text_writer.offset());
      put(register_file.clobbered);
    }
    RetBuilder rb{*this,*cur_cc_assigner()};
    rb.ret();
    return true;
  }
};

static void abi(const std::vector<Operation> &operations) {
  a64::CCAssignerAAPCS assigner;
  for(const auto &operation:operations) {
    const auto &a=operation.args;
    if(operation.code==0) {assigner.reset();put(assigner.get_stack_size());continue;}
    if(operation.code==3) {
      const auto info=assigner.get_ccinfo();
      put(info.allocatable_regs);put(info.callee_saved_regs);put(info.arg_regs);put(info.red_zone_size);
      continue;
    }
    CCAssignment assignment{};
    assignment.bank=RegBank{u8(a[0])};assignment.size=a[1];assignment.align=a[2];
    assignment.consecutive=a[3];assignment.sret=a[4]&1;assignment.byval=a[4]&2;
    assignment.int_ext=a[5];assignment.reg=Reg{a[6]};assignment.stack_off=a[7];
    if(operation.code==1) assigner.assign_arg(assignment);
    else if(operation.code==2) assigner.assign_ret(assignment);
    else throw std::runtime_error("unknown ABI operation");
    put(assignment.reg.id());put(assignment.stack_off);put(assigner.get_stack_size());
  }
}

int main() {
  try {
    const u64 mode=number(),count=number();
    std::vector<Operation> operations;
    for(u64 index=0;index<count;++index) {
      Operation operation{number(),{}};
      for(auto &value:operation.args) value=number();
      operations.push_back(operation);
    }
    if(std::cin.peek()!=EOF) throw std::runtime_error("unexpected trailing input");
    if(mode==1) {abi(operations);return 0;}
    if(mode!=0) throw std::runtime_error("unknown raw protocol mode");
    test::TestIR ir;
    if(!ir.parse_ir("raw() {\nentry:\nterminate\n}\n")) throw std::runtime_error("raw IR parse failed");
    test::TestIRAdaptor adaptor{&ir};
    RawCompiler compiler{&adaptor,operations};
    if(!compiler.compile()) throw std::runtime_error("raw compiler failed");
    const auto object=compiler.assembler.build_object_file();
    put(object.size());
    std::cout.write(reinterpret_cast<const char *>(object.data()),object.size());
    return 0;
  } catch(const std::exception &error) {
    std::cerr<<error.what()<<'\n';return 2;
  }
}
