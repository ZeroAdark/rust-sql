// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <array>
#include <bit>
#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <limits>
#include <vector>
#include <span>
#include "tpde/util/VectorWriter.hpp"
using namespace tpde;
using namespace tpde::util;
static uint64_t read(unsigned n) {
  uint64_t v=0;for(unsigned i=0;i<n;i++){int c=std::cin.get();if(c<0)std::abort();v|=uint64_t(c)<<(8*i);}return v;
}
static void out(uint64_t v){for(unsigned i=0;i<8;i++)std::cout.put(char(v>>(8*i)));}
static void bytes(const u8 *p,size_t n){std::cout.write(reinterpret_cast<const char *>(p),n);}
template<class T> static void unsigned_case(u64 value) {
  T v=T(value)|T(1);out(cnt_tz(v));out(cnt_lz(v));
  T a=T(value & (std::numeric_limits<T>::max()>>1));out(align_down(a,T(8)));out(align_up(a,T(8)));
}
template<class T> static void signed_case(u64 value) {
  T v=T(value&63)-T(31);out(u64(align_down(v,T(8))));out(u64(align_up(v,T(8))));
}
static void numbers() {
  auto n=read(4);for(size_t i=0;i<n;i++){
    u64 v=read(8);unsigned bits=read(1);
    unsigned_case<u8>(v);unsigned_case<u16>(v);unsigned_case<u32>(v);unsigned_case<u64>(v);
    signed_case<i8>(v);signed_case<i16>(v);signed_case<i32>(v);signed_case<i64>(v);
    out(zext(v,bits));out(u64(sext(v,bits)));
    std::array<u8,10> b;unsigned len=uleb_write(b.data(),v);out(uleb_len(v));out(len);bytes(b.data(),len);
    len=sleb_write(b.data(),i64(v));out(len);bytes(b.data(),len);
    out(std::popcount(v));for(auto bit:BitSetIterator<false>(v))out(bit);
    out(std::popcount(v));for(auto bit:BitSetIterator<true>(v))out(bit);
  }
}
static void integer(VectorWriter &w,unsigned kind,u64 v,bool checked) {
#define WRITE(T,V) if(checked)w.write<T>(V);else w.write_unchecked<T>(V)
  switch(kind){
    case 0:{WRITE(bool,v!=0);break;} case 1:{WRITE(u8,u8(v));break;}
    case 2:{WRITE(u16,u16(v));break;} case 3:{WRITE(u32,u32(v));break;}
    case 4:{WRITE(u64,v);break;} case 5:{WRITE(size_t,size_t(v));break;}
    case 6:{WRITE(i8,i8(v));break;} case 7:{WRITE(i16,i16(v));break;}
    case 8:{WRITE(i32,i32(v));break;} case 9:{WRITE(i64,i64(v));break;}
    case 10:{WRITE(ptrdiff_t,ptrdiff_t(v));break;}default:std::abort();
  }
#undef WRITE
}
static void writer() {
  SmallVector<u8,8> old;old.resize(8,0x33);
  SmallVector<u8,8> v;size_t initial=read(4);v.resize(initial);for(auto &b:v)b=u8(read(1));
  auto offset=read(4);
  {
    VectorWriter w=offset==0xffffffff ? VectorWriter(v):VectorWriter(v,offset);
    while(true){unsigned op=read(1);if(op==255)break;
      switch(op){
        case 1:w.reserve(read(4));break;
        case 2:case 10:{size_t n=read(4);std::vector<u8>b(n);for(auto &x:b)x=u8(read(1));if(op==2)w.write(std::span<const u8>(b));else w.write_unchecked(std::span<const u8>(b));break;}
        case 3:w.zero(read(4));break;
        case 4:case 12:{size_t n=read(4),off=w.size();if(op==4)w.skip(n);else w.skip_unchecked(n);std::memset(w.data()+off,0x5a,n);break;}
        case 5:w.unskip(read(4));break;
        case 6:w.flush();break;
        case 7:case 18:{unsigned kind=read(1);u64 value=read(8);integer(w,kind,value,op==7);break;}
        case 8:w.write_uleb(read(8));break;
        case 9:w.write_sleb(i64(read(8)));break;
        case 11:w.zero_unchecked(read(4));break;
        case 13:w.write_uleb_unchecked(read(8));break;
        case 14:w.write_sleb_unchecked(i64(read(8)));break;
        case 15:{VectorWriter moved(std::move(w));w=std::move(moved);break;}
        case 16:{{VectorWriter destination(old);destination.unskip(3);destination=std::move(w);w=std::move(destination);}break;}
        case 17:w.skip_unchecked(read(4));break;
        default:std::abort();
      }
      out(w.size());out(w.capacity());bytes(w.data(),w.size());
    }
  }
  out(v.size());out(v.capacity());bytes(v.data(),v.size());out(old.size());bytes(old.data(),old.size());
}
int main(){unsigned mode=read(1);if(mode==0)numbers();else if(mode==1)writer();else std::abort();}
