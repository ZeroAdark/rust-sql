// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#include "tpde/util/function_ref.hpp"
#include <bit>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <type_traits>

using u64 = std::uint64_t;
using tpde::util::function_ref;

static u64 read(unsigned bytes) {
  u64 value = 0;
  for (unsigned i = 0; i < bytes; ++i) {
    const int byte = std::getchar();
    if (byte == EOF) std::abort();
    value |= u64(byte) << (i * 8);
  }
  return value;
}
static void emit(u64 value) {
  for (unsigned i = 0; i < 8; ++i) std::putchar((value >> (i * 8)) & 255);
}
static u64 affine(u64 value) { return value * 3 + 7; }
static u64 alternate(u64 value) { return (value ^ 0x938fad08533d5821ULL) * 5; }
static const u64 &identity(const u64 &value) { return value; }
struct Accessor {
  u64 state;
  u64 &operator()() { return state; }
};

static_assert(std::is_copy_constructible_v<function_ref<u64(u64)>>);
static_assert(!std::is_default_constructible_v<function_ref<u64(u64)>>);
static_assert(std::is_trivially_destructible_v<function_ref<u64(u64)>>);
static_assert(!std::is_constructible_v<function_ref<u64(const char *)>, decltype(affine) *>);

int main() {
  const auto count = read(4);
  for (u64 i = 0; i < count; ++i) {
    const auto seed = read(8);
    const auto salt = std::rotl(seed, 7);
    auto pointer = &affine;
    emit(function_ref<u64(u64)>(pointer)(seed));
    const auto captured = [salt](u64 value) { return value ^ salt; };
    const function_ref<u64(u64)> original(captured);
    const auto copied(original);
    emit(copied(seed));
    auto pair = [salt](u64 a, u64 b) { return a * b + salt; };
    emit(function_ref<u64(u64, u64)>(pair)(seed, 17));
    auto many = [](u64 a, u64 b, u64 c, u64 d, u64 e, u64 f, u64 g, u64 h, u64 i) {
      return a + b + c + d + e + f + g + h + i;
    };
    emit(function_ref<u64(u64, u64, u64, u64, u64, u64, u64, u64, u64)>(many)(seed, 1, 2, 3, 4, 5, 6, 7, 8));
    auto zero = [seed]() { return seed; };
    emit(function_ref<u64()>(zero)());
    auto identity_pointer = &identity;
    const function_ref<const u64 &(const u64 &)> reference(identity_pointer);
    emit(reference(seed));
    emit(&reference(seed) == &seed);
    auto update = [salt](u64 &value) { value ^= salt; };
    auto value = seed;
    const function_ref<void(u64 &)> updater(update);
    updater(value);
    emit(value);
    auto consume = [salt](std::unique_ptr<u64> value) { return *value + salt; };
    emit(function_ref<u64(std::unique_ptr<u64>)>(consume)(std::make_unique<u64>(seed)));
    auto mutable_capture = [state = seed](u64 value) mutable { return state += value; };
    const function_ref<u64(u64)> mut_original(mutable_capture);
    const auto mut_copied(mut_original);
    emit(mut_original(1));
    emit(mut_copied(salt));
    emit(mut_copied(seed));
    Accessor accessor{seed};
    const function_ref<u64 &()> ref_original(accessor);
    const auto ref_copied(ref_original);
    auto &first = ref_original();
    first ^= salt;
    emit(first);
    auto &second = ref_copied();
    second += 1;
    emit(second);
    emit(&first == &second);
    auto external = seed;
    auto update_external = [&external](u64 value) { external ^= value; };
    const function_ref<void(u64)> external_original(update_external);
    const auto external_copied(external_original);
    external_original(salt);
    external_copied(17);
    emit(external);
    auto first_pointer = &affine;
    auto second_pointer = &alternate;
    function_ref<u64(u64)> rebound(first_pointer);
    const auto nested(rebound);
    emit(nested(seed));
    const auto snapshot(static_cast<const function_ref<u64(u64)> &>(rebound));
    const auto forwarded(rebound);
    emit(forwarded(seed));
    const function_ref<u64(u64)> replacement(second_pointer);
    rebound = replacement;
    emit(forwarded(seed));
    emit(snapshot(seed));
    emit(function_ref<u64(u64)>{std::move(function_ref<u64(u64)>{first_pointer})}(seed));
  }
  return std::ferror(stdout) ? 1 : 0;
}
