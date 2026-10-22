// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include <cassert>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iterator>
#include <memory>
#include <new>
#include <type_traits>
#include <utility>

#include "tpde/util/BumpAllocator.hpp"

namespace {
struct Slab {
  void *data;
  size_t size;
  bool live;
};

Slab slabs[65536];
size_t total_slabs;
size_t live_slabs;
size_t live_bytes;
size_t destroyed;

void require(bool condition) {
  if (!condition) std::abort();
}

size_t locate(const void *data) {
  const auto address = reinterpret_cast<uintptr_t>(data);
  for (size_t i = 0; i < total_slabs; ++i) {
    const auto begin = reinterpret_cast<uintptr_t>(slabs[i].data);
    if (slabs[i].live && address >= begin && address - begin < slabs[i].size)
      return i;
  }
  std::abort();
}

void release(void *data) {
  if (!data) return;
  const size_t id = locate(data);
  require(data == slabs[id].data);
  slabs[id].live = false;
  --live_slabs;
  live_bytes -= slabs[id].size;
  std::free(data);
}

void snapshot(char op, void *data, size_t size, size_t align) {
  const size_t id = locate(data);
  const auto offset = reinterpret_cast<uintptr_t>(data) -
                      reinterpret_cast<uintptr_t>(slabs[id].data);
  require(offset <= slabs[id].size && size <= slabs[id].size - offset);
  require(reinterpret_cast<uintptr_t>(data) % align == 0);
  std::memset(data, 0x5a, size);
  std::printf("%c %zu %zu %zu %zu %zu\n", op, id, offset, slabs[id].size,
              live_slabs, live_bytes);
}

struct Tracked {
  uint64_t value;
  ~Tracked() { ++destroyed; }
};

struct Throwing {
  uint64_t value;
  Throwing() : value(0) { throw 7; }
};

static_assert(sizeof(Tracked) == 8 && alignof(Tracked) == 8);
static_assert(sizeof(Throwing) == 8 && std::is_trivially_destructible_v<Throwing>);
static_assert(alignof(std::max_align_t) == 16);

template <size_t SlabSize>
void lifetime_cases(tpde::util::BumpAllocator<SlabSize> &allocator) {
  destroyed = 0;
  tpde::util::BumpAllocUniquePtr<Tracked> first(new (allocator) Tracked{41});
  tpde::util::BumpAllocUniquePtr<Tracked> moved(std::move(first));
  const bool move_ok = !first && moved->value == 41;
  moved.reset();
  tpde::util::BumpAllocUniquePtr<Tracked> second(new (allocator) Tracked{43});
  auto *released = second.release();
  const auto released_value = released->value;
  tpde::util::BumpAllocatorDeleter<Tracked>{}(released);
  auto *array = new (allocator) uint64_t[3]{2, 3, 5};
  const auto sum = array[0] + array[1] + array[2];
  operator delete[](array, allocator);
  unsigned thrown = 0;
  try {
    (void)new (allocator) Throwing;
  } catch (int value) {
    require(value == 7);
    ++thrown;
  }
  try {
    (void)new (allocator) Throwing[2];
  } catch (int value) {
    require(value == 7);
    ++thrown;
  }
  require(move_ok && released_value == 43 && sum == 10 && thrown == 2 &&
          destroyed == 2);
  std::printf("D %u %llu %llu %u %zu %zu %zu\n", unsigned(move_ok),
              static_cast<unsigned long long>(released_value),
              static_cast<unsigned long long>(sum), thrown, destroyed,
              live_slabs, live_bytes);
}
} // namespace

void *operator new(size_t size, std::align_val_t alignment) {
  void *data = nullptr;
  if (posix_memalign(&data, size_t(alignment), size) != 0) throw std::bad_alloc();
  require(total_slabs < std::size(slabs));
  slabs[total_slabs++] = Slab{data, size, true};
  ++live_slabs;
  live_bytes += size;
  return data;
}

void operator delete(void *data, std::align_val_t) noexcept { release(data); }

void operator delete(void *data, size_t size, std::align_val_t) noexcept {
  if (data) require(slabs[locate(data)].size == size);
  release(data);
}

#ifdef TPDE_ASAN_BUILD
extern "C" int __asan_address_is_poisoned(void const volatile *address);
#endif

template <size_t SlabSize>
void run() {
  {
    tpde::util::BumpAllocator<SlabSize> allocator;
    char op;
    while (std::scanf(" %c", &op) == 1) {
      switch (op) {
      case 'A':
      case 'S': {
        size_t size, align;
        require(std::scanf("%zu %zu", &size, &align) == 2);
        require(size > 0 && align > 0 && (align & (align - 1)) == 0 &&
                align <= alignof(std::max_align_t));
        void *data = op == 'A' ? allocator.allocate(size, align)
                               : allocator.allocate_slab(size, align);
        snapshot(op, data, size, align);
        break;
      }
      case 'R':
        allocator.reset();
        std::printf("R %zu %zu\n", live_slabs, live_bytes);
        break;
      case 'D':
        lifetime_cases(allocator);
        break;
      case 'P': {
        size_t id, offset;
        require(std::scanf("%zu %zu", &id, &offset) == 2);
        require(id < total_slabs && slabs[id].live && offset < slabs[id].size);
#ifdef TPDE_ASAN_BUILD
        const auto *data = static_cast<const unsigned char *>(slabs[id].data);
        std::printf("P %u\n", unsigned(__asan_address_is_poisoned(data + offset) != 0));
#else
        std::abort();
#endif
        break;
      }
      default:
        std::abort();
      }
    }
    require(std::feof(stdin));
  }
  require(live_slabs == 0 && live_bytes == 0);
  std::puts("E 0 0");
}

int main() {
  size_t slab_size;
  require(std::scanf("%zu", &slab_size) == 1);
  switch (slab_size) {
  case 64: run<64>(); break;
  case 128: run<128>(); break;
  case 4096: run<4096>(); break;
  default: std::abort();
  }
}
