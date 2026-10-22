// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <bit>
#include <cstddef>
#include <cstdint>
#include <iostream>
#include <limits>
#include <span>
#include <string>
#include <tuple>
#include <vector>
#include "tpde/Analyzer.hpp"

using tpde::u32;
struct Value { u32 local_idx; bool ignore; std::vector<std::pair<u32,u32>> incoming; };
struct Block { std::vector<u32> succs,insts,phis; u32 info=0,info2=0; };
struct Inst { std::vector<u32> operands,results; bool fused; };
struct Program { std::string name; u32 epoch,entry,highest; std::vector<u32> args,block_ids; std::vector<Value> values; std::vector<Block> blocks; std::vector<Inst> insts; };
struct Phi {
  std::span<const std::pair<u32,u32>> incoming;
  u32 incoming_count() const {return incoming.size();}
  u32 incoming_val_for_slot(u32 slot) const {return incoming[slot].second;}
  u32 incoming_block_for_slot(u32 slot) const {return incoming[slot].first;}
  u32 incoming_val_for_block(u32 block) const {for(auto p:incoming) if(p.first==block)return p.second; std::abort();}
};
template<bool Highest,bool Visit> struct Fixture {
  using IRValueRef=u32; using IRBlockRef=u32; using IRInstRef=u32; using IRFuncRef=u32;
  static constexpr u32 INVALID_VALUE_REF=~0u,INVALID_BLOCK_REF=~0u,INVALID_FUNC_REF=~0u;
  static constexpr bool TPDE_PROVIDES_HIGHEST_VAL_IDX=Highest,TPDE_LIVENESS_VISIT_ARGS=Visit;
  Program *p;
  std::array<u32,1> funcs()const{return {0};}
  std::array<u32,1> funcs_to_compile()const{return {0};}
  u32 func_count()const{return 1;}
  std::string_view func_link_name(u32)const{return "fixture";}
  bool func_extern(u32)const{return false;}
  bool func_only_local(u32)const{return true;}
  bool func_has_weak_linkage(u32)const{return false;}
  bool cur_needs_unwind_info()const{return false;}
  bool cur_is_vararg()const{return false;}
  u32 cur_highest_val_idx()const requires(Highest) {return p->highest;}
  auto cur_args()const{return std::span<const u32>(p->args);}
  bool cur_arg_is_byval(u32)const{return false;}
  u32 cur_arg_byval_size(u32)const{return 0;}
  u32 cur_arg_byval_align(u32)const{return 1;}
  bool cur_arg_is_sret(u32)const{return false;}
  std::span<const u32> cur_static_allocas()const{return {};}
  bool cur_has_dynamic_alloca()const{return false;}
  u32 cur_entry_block()const{return p->entry;}
  auto cur_blocks()const{return std::span<const u32>(p->block_ids);}
  auto block_succs(u32 b)const{return std::span<const u32>(p->blocks[b].succs);}
  auto block_insts(u32 b)const{return std::span<const u32>(p->blocks[b].insts);}
  auto block_phis(u32 b)const{return std::span<const u32>(p->blocks[b].phis);}
  u32 block_info(u32 b)const{return p->blocks[b].info;}
  void block_set_info(u32 b,u32 info){p->blocks[b].info=info;}
  u32 block_info2(u32 b)const{return p->blocks[b].info2;}
  void block_set_info2(u32 b,u32 info){p->blocks[b].info2=info;}
  u32 block_fmt_ref(u32 b)const{return b;}
  tpde::ValLocalIdx val_local_idx(u32 v)const{return static_cast<tpde::ValLocalIdx>(p->values[v].local_idx);}
  bool val_ignore_in_liveness_analysis(u32 v)const{return p->values[v].ignore;}
  bool val_is_phi(u32 v)const{for(const auto&b:p->blocks)if(std::find(b.phis.begin(),b.phis.end(),v)!=b.phis.end())return true;return false;}
  Phi val_as_phi(u32 v)const{return {p->values[v].incoming};}
  u32 val_alloca_size(u32)const{return 0;}
  u32 val_alloca_align(u32)const{return 1;}
  u32 value_fmt_ref(u32 v)const{return v;}
  auto inst_operands(u32 i)const{return std::span<const u32>(p->insts[i].operands);}
  auto inst_results(u32 i)const{return std::span<const u32>(p->insts[i].results);}
  bool inst_fused(u32 i)const{return p->insts[i].fused;}
  u32 inst_fmt_ref(u32 i)const{return i;}
  void start_compile(){}
  void end_compile(){}
  bool switch_func(u32 f){return f==0;}
  void reset(){}
};
static_assert(tpde::IRAdaptor<Fixture<true,true>>);
static_assert(tpde::IRAdaptor<Fixture<false,false>>);
void read_words(std::vector<u32>&values) {
  u32 size; std::cin>>size; values.resize(size); for(auto &v:values)std::cin>>v;
}
void read(Program &p) {
  u32 blocks,values,insts;
  std::cin>>p.name>>p.epoch>>p.entry>>p.highest>>blocks>>values>>insts;
  read_words(p.args);
  p.values.resize(values); p.blocks.clear(); p.blocks.resize(blocks); p.insts.resize(insts);
  p.block_ids.resize(blocks); for(u32 i=0;i<blocks;++i)p.block_ids[i]=i;
  for(auto &v:p.values) {
    u32 count; std::cin>>v.local_idx>>v.ignore>>count;
    v.incoming.resize(count); for(auto &pair:v.incoming) std::cin>>pair.first>>pair.second;
  }
  for(auto &b:p.blocks){read_words(b.succs);read_words(b.insts);read_words(b.phis);}
  for(auto &i:p.insts){std::cin>>i.fused;read_words(i.operands);read_words(i.results);}
  if(!std::cin)std::abort();
}
template<class Range> void words(const Range &range) {
  std::cout<<range.size(); for(auto value:range) std::cout<<' '<<value; std::cout<<'\n';
}
template<bool Highest,bool Visit> void run(u32 count) {
  Program p;
  Fixture<Highest,Visit> fixture{&p};
  tpde::Analyzer<Fixture<Highest,Visit>> analyzer(&fixture);
  using Index=typename decltype(analyzer)::BlockIndex;
  for(u32 n=0;n<count;++n) {
    read(p);
    std::cerr<<"case "<<p.name<<'\n';
    if(p.epoch<=65535)analyzer.liveness_epoch=p.epoch;
    analyzer.switch_func(0);
    std::cout<<"case "<<p.name<<'\n';
    std::cout<<"state "<<analyzer.liveness_epoch<<' '<<analyzer.liveness_max_value<<' '<<analyzer.num_insts<<'\n';
    std::cout<<"layout "; words(analyzer.block_layout);
    std::cout<<"map "; words(analyzer.block_loop_map);
    std::cout<<"loops "<<analyzer.loops.size()<<'\n';
    for(u32 i=0;i<analyzer.loops.size();++i) {
      const auto &l=analyzer.loop_from_idx(i);
      std::cout<<"loop "<<i<<' '<<l.level<<' '<<l.parent<<' '<<static_cast<u32>(l.begin)<<' '<<static_cast<u32>(l.end)<<' '<<l.num_blocks<<' '<<l.definitions<<' '<<l.definitions_in_childs<<'\n';
    }
    std::cout<<"values "<<analyzer.liveness_max_value+1<<'\n';
    for(u32 i=0;i<=analyzer.liveness_max_value;++i) {
      const auto &l=analyzer.liveness[i];
      if(l.epoch==analyzer.liveness_epoch) {
        const auto &checked=analyzer.liveness_info(static_cast<tpde::ValLocalIdx>(i));
        std::cout<<"live "<<i<<' '<<checked.epoch<<' '<<static_cast<u32>(l.first)<<' '<<static_cast<u32>(l.last)<<' '<<l.ref_count<<' '<<l.lowest_common_loop<<' '<<l.last_full<<'\n';
      } else {std::cout<<"ignored "<<i<<' '<<l.epoch<<'\n';}
    }
    std::cout<<"next "<<analyzer.block_ref(static_cast<Index>(analyzer.block_layout.size()))<<'\n';
    std::vector<std::pair<u32,u32>> before; for(const auto&b:p.blocks)before.emplace_back(b.info,b.info2);
    std::cout<<"rpo\n"; analyzer.print_rpo(std::cout);
    for(u32 i=0;i<p.blocks.size();++i)assert(before[i]==std::make_pair(p.blocks[i].info,p.blocks[i].info2));
    std::cout<<"layout_print\n"; analyzer.print_block_layout(std::cout);
    std::cout<<"loops_print\n"; analyzer.print_loops(std::cout);
    std::cout<<"liveness_print\n"; analyzer.print_liveness(std::cout);
    std::cout<<"metadata "<<p.blocks.size()<<'\n';
    for(u32 i=0;i<p.blocks.size();++i) {
      const auto &b=p.blocks[i];
      std::cout<<"block "<<i<<' '<<(b.info2==0?~0u:b.info)<<' '<<b.info2<<' '<<analyzer.block_has_multiple_incoming(i)<<' '<<analyzer.block_has_phis(i)<<'\n';
    }
    std::cout<<"end\n";
  }
}
int main() {
  bool highest,visit;u32 count;std::cin>>highest>>visit>>count;
  if(highest){if(visit)run<true,true>(count);else run<true,false>(count);}
  else{if(visit)run<false,true>(count);else run<false,false>(count);}
}
