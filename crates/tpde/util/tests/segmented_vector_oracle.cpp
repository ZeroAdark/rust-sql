// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <cassert>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <utility>
#include <vector>
#include "tpde/util/SegmentedVector.hpp"

struct Event { uint64_t kind, id, value; };
using Events = std::vector<Event>;

struct alignas(128) Tracked {
  uint64_t id, value;
  Events *events;
  bool active = true;
  Tracked(uint64_t id, uint64_t value, Events &events)
      : id(id), value(value), events(&events) {
    events.push_back({1, id, value});
  }
  Tracked(const Tracked &other)
      : id(other.id), value(other.value), events(other.events) {
    events->push_back({2, id, value});
  }
  Tracked(Tracked &&other)
      : id(other.id), value(other.value), events(other.events) {
    other.active = false;
  }
  ~Tracked() {
    if (active) events->push_back({3, id, value});
  }
};

static uint64_t read(unsigned bytes) {
  uint64_t value = 0;
  for (unsigned i = 0; i < bytes; ++i) {
    const int byte = std::getchar();
    if (byte == EOF) std::abort();
    value |= uint64_t(byte) << (i * 8);
  }
  return value;
}

static void write(uint64_t value) {
  for (unsigned i = 0; i < 8; ++i) std::putchar(value >> (i * 8));
}

static void events_snapshot(const Events &events) {
  write(events.size());
  for (const auto &event : events) {
    write(event.kind); write(event.id); write(event.value);
  }
}

template <size_t N>
static void snapshot(tpde::util::SegmentedVector<Tracked, N> &set,
                     std::vector<uintptr_t> &addresses,
                     const Events &events) {
  const auto &immutable = set;
  write(set.size());
  for (size_t i = 0; i < set.size(); ++i) {
    const auto &element = immutable[i];
    const auto pointer = reinterpret_cast<uintptr_t>(&element);
    write(element.id);
    write(element.value);
    write(pointer % alignof(Tracked) == 0);
    write(i % N == 0 || pointer == addresses[i - 1] + sizeof(Tracked));
    if (i == addresses.size()) addresses.push_back(pointer);
    write(pointer == addresses[i]);
    write(&element == &immutable[i]);
    write(&element == &set[i]);
  }
  events_snapshot(events);
}

template <size_t N>
static void run(unsigned operations) {
  Events events;
  std::vector<uintptr_t> addresses;
  uint64_t next_id = 1;
  {
    tpde::util::SegmentedVector<Tracked, N> set;
    snapshot(set, addresses, events);
    for (unsigned step = 0; step < operations; ++step) {
      const auto op = read(1), n = read(4), value = read(8);
      switch (op) {
      case 0: break;
      case 1: set.clear(); break;
      case 2:
      case 3:
        for (uint64_t j = 0; j < n; ++j) {
          if (op == 2) {
            set.push_back(Tracked(next_id, value + j, events));
          } else {
            auto &inserted = set.emplace_back(next_id, value + j, events);
            if (inserted.id != next_id) std::abort();
          }
          ++next_id;
        }
        break;
      case 4: {
        const Tracked source(next_id++, value, events);
        for (uint64_t j = 0; j < n; ++j) set.push_back(source);
        break;
      }
      case 5: set[n].value = value; break;
      case 6: {
        const auto &first = set[0];
        const auto first_id = first.id;
        for (uint64_t j = 0; j < n; ++j) {
          set.push_back(Tracked(next_id++, value + j, events));
        }
        if (first.id != first_id || &first != &set[0]) std::abort();
        break;
      }
      default: std::abort();
      }
      snapshot(set, addresses, events);
    }
  }
  events_snapshot(events);
}

int main() {
  const auto size = read(1), operations = read(4);
  switch (size) {
  case 1: run<1>(operations); break;
  case 3: run<3>(operations); break;
  case 64: run<64>(operations); break;
  default: std::abort();
  }
}
