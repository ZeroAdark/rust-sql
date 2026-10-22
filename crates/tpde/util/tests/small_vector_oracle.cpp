// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdlib>
#include <iostream>
#include <iterator>
#include <limits>
#include <memory>
#include <ranges>
#include <type_traits>
#include <vector>
#include "tpde/util/SmallVector.hpp"

using namespace tpde::util;

static uint64_t read(unsigned bytes) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) {
    int c = std::cin.get();
    if (c < 0) std::abort();
    value |= uint64_t(c) << (i * 8);
  }
  return value;
}
static void out(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::cout.put(char(value >> (i * 8)));
}
struct Stats {
  uint64_t created = 0, released = 0;
};
static Stats *active;
struct Tracked {
  std::unique_ptr<int64_t> resource;
  Stats *stats;
  Tracked(int64_t value = 0)
      : resource(std::make_unique<int64_t>(value)), stats(active) { ++stats->created; }
  Tracked(const Tracked &other) : Tracked(*other.resource) {}
  Tracked(Tracked &&other) noexcept
      : resource(std::move(other.resource)), stats(other.stats) {}
  Tracked &operator=(Tracked &&other) noexcept {
    if (this != &other) {
      if (resource) ++stats->released;
      resource = std::move(other.resource);
      stats = other.stats;
    }
    return *this;
  }
  ~Tracked() { if (resource) ++stats->released; }
};
static int64_t value(const int64_t &item) { return item; }
static int64_t value(const Tracked &item) { return *item.resource; }
static void set(int64_t &item, int64_t v) { item = v; }
static void set(Tracked &item, int64_t v) { *item.resource = v; }
template<class T> static T make(uint64_t v) { return T(int64_t(v)); }
template<class T> static void state(SmallVectorBase<T> &v) {
  const auto &c = v;
  out(c.size()); out(c.capacity()); out(c.empty()); out(c.max_size());
  out(c.empty() ? UINT64_MAX : uint64_t(value(c.front())));
  out(c.empty() ? UINT64_MAX : uint64_t(value(c.back())));
  out(c.end() - c.begin()); out(c.cend() - c.cbegin());
  out(v.data() == v.begin()); out(c.data() == c.cbegin());
  for (size_t i = 0; i < c.size(); ++i) out(uint64_t(value(c[i])));
  for (auto it = v.begin(); it != v.end(); ++it) out(uint64_t(value(*it)));
  for (auto it = c.cend(); it != c.cbegin();) out(uint64_t(value(*--it)));
}
template<class T, size_t N> static void script() {
  const auto count = read(4);
  Stats stats;
  active = &stats;
  {
    SmallVector<T, N> vectors[2];
    auto snapshot = [&] {
      state<T>(vectors[0]); state<T>(vectors[1]);
      out(stats.created); out(stats.released);
    };
    snapshot();
    for (size_t step = 0; step < count; ++step) {
      unsigned op = read(1), a = read(1), b = read(1);
      read(1);
      size_t n = read(4), m = read(4);
      uint64_t v = read(8);
      auto &dst = vectors[a];
      switch (op) {
      case 0: break;
      case 1: dst.clear(); break;
      case 2: dst.reserve(n); break;
      case 3: dst.push_back(make<T>(v)); break;
      case 4: { const T item = make<T>(v); dst.push_back(item); break; }
      case 5: { auto &item = dst.emplace_back(int64_t(v)); set(item, int64_t(v ^ 0x5678)); break; }
      case 6: dst.pop_back(); break;
      case 7: { const T item = make<T>(v); dst.resize(n, item); break; }
      case 8: dst.resize(n); break;
      case 9: {
        auto result = dst.erase(dst.begin() + n, dst.begin() + m);
        out(result - dst.begin());
        break;
      }
      case 10: case 11: case 17: case 18: {
        std::vector<T> items;
        for (size_t i = 0; i < n; ++i) items.emplace_back(int64_t(v + i * UINT64_C(0x9e3779b97f4a7c15)));
        if (op == 10) dst.append(std::make_move_iterator(items.begin()), std::make_move_iterator(items.end()));
        else if (op == 11) dst.append(items.cbegin(), items.cend());
        else {
          auto selected = items | std::views::filter([](const T &item) { return (value(item) & 1) == 0; });
          dst.append(selected.begin(), selected.end());
        }
        break;
      }
      case 12: dst = std::move(vectors[b]); break;
      case 13:
        dst.~SmallVector<T, N>();
        ::new (&dst) SmallVector<T, N>(std::move(vectors[b]));
        break;
      case 14: set(dst[n], int64_t(v)); break;
      case 15: {
        size_t old = dst.size();
        dst.resize_uninitialized(n);
        for (size_t i = old; i < n; ++i) set(dst[i], int64_t(v + i - old));
        break;
      }
      case 16: if (!dst.empty()) { out(uint64_t(value(dst.back()))); dst.pop_back(); } else out(UINT64_MAX); break;
      case 19: dst.~SmallVector<T, N>(); ::new (&dst) SmallVector<T, N>(); break;
      default: std::abort();
      }
      snapshot();
    }
  }
  out(stats.created); out(stats.released);
  if (stats.created != stats.released) std::abort();
}
template<class T> static void dispatch(unsigned n) {
  switch (n) {
  case 0: script<T, 0>(); break;
  case 1: script<T, 1>(); break;
  case 4: script<T, 4>(); break;
  case 32: script<T, 32>(); break;
  default: std::abort();
  }
}
template<size_t N> struct Bytes { unsigned char data[N]; };
static void dimensions() {
  out(SmallVectorDefaultSize<uint8_t>);
  out(SmallVectorDefaultSize<uint64_t>);
  out(SmallVectorDefaultSize<Bytes<255>>);
  out(SmallVectorDefaultSize<Bytes<256>>);
  out(SmallVectorDefaultSize<Bytes<257>>);
  out(SmallVector<uint8_t>::max_size());
  out(SmallVector<uint64_t>::max_size());
  out(SmallVector<Bytes<257>>::max_size());
  static_assert(std::is_same_v<SmallVector<int64_t>::size_type, size_t>);
  static_assert(std::is_same_v<SmallVector<int64_t>::difference_type, ptrdiff_t>);
  static_assert(std::is_same_v<SmallVector<int64_t>::iterator, int64_t *>);
  static_assert(std::is_same_v<SmallVector<int64_t>::const_iterator, const int64_t *>);
  static_assert(!std::is_copy_constructible_v<SmallVector<int64_t>>);
  static_assert(!std::is_copy_assignable_v<SmallVector<int64_t>>);
  static_assert(SmallVector<int64_t>::IsTrivial);
  static_assert(!SmallVector<Tracked>::IsTrivial);
}
template<size_t A, size_t B> static void cross() {
  SmallVector<int64_t, A> a;
  SmallVector<int64_t, B> b;
  for (size_t count : {size_t(0), size_t(1), size_t(4), size_t(5), size_t(17)}) {
    a.~SmallVector<int64_t, A>(); ::new (&a) SmallVector<int64_t, A>();
    b.~SmallVector<int64_t, B>(); ::new (&b) SmallVector<int64_t, B>();
    for (size_t i = 0; i < count; ++i) b.push_back(int64_t(i * 19));
    a = std::move(static_cast<SmallVectorBase<int64_t> &>(b));
    state<int64_t>(a); state<int64_t>(b);
    b.push_back(-9);
    SmallVector<int64_t, B> c(std::move(static_cast<SmallVectorBase<int64_t> &>(a)));
    state<int64_t>(a); state<int64_t>(b); state<int64_t>(c);
    a.push_back(-11);
    state<int64_t>(a);
  }
}
template<class T> static void allocation_failure(bool heap) {
  Stats stats;
  active = &stats;
  {
    SmallVector<T, 4> v;
    v.emplace_back(7);
    if (heap) v.reserve(8);
    state<T>(v);
    bool failed = false;
    try { v.reserve(size_t(PTRDIFF_MAX) / sizeof(T)); }
    catch (const std::bad_alloc &) { failed = true; }
    out(failed);
    state<T>(v);
    out(stats.created); out(stats.released);
  }
  out(stats.created); out(stats.released);
}
struct MoveOnly : Tracked {
  using Tracked::Tracked;
  MoveOnly(const MoveOnly &) = delete;
  MoveOnly(MoveOnly &&) = default;
};
static void forward_move() {
  Stats stats;
  active = &stats;
  {
    std::array source{MoveOnly(10), MoveOnly(11), MoveOnly(12), MoveOnly(13), MoveOnly(14)};
    SmallVector<MoveOnly, 1> destination;
    auto selected = source | std::views::filter([](const MoveOnly &item) { return (value(item) & 1) == 0; });
    destination.append(std::make_move_iterator(selected.begin()), std::make_move_iterator(selected.end()));
    out(destination.size()); out(destination.capacity());
    for (const auto &item : destination) out(uint64_t(value(item)));
    for (const auto &item : source) {
      out(bool(item.resource));
      out(item.resource ? uint64_t(value(item)) : UINT64_MAX);
    }
    out(stats.created); out(stats.released);
    destination.clear();
    out(stats.created); out(stats.released);
  }
  out(stats.created); out(stats.released);
}
int main() {
  unsigned mode = read(1);
  if (mode == 0) dispatch<int64_t>(read(1));
  else if (mode == 1) dispatch<Tracked>(read(1));
  else if (mode == 2) dimensions();
  else if (mode == 3) { cross<0, 4>(); cross<4, 0>(); cross<1, 32>(); cross<32, 1>(); }
  else if (mode == 4) {
    allocation_failure<int64_t>(false); allocation_failure<int64_t>(true);
    allocation_failure<Tracked>(false); allocation_failure<Tracked>(true);
  }
  else if (mode == 5) forward_move();
  else std::abort();
}
