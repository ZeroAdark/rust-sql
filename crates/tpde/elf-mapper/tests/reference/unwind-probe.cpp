// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#ifndef _GNU_SOURCE
#define _GNU_SOURCE 1
#endif
#include <atomic>
#include <cerrno>
#include <cinttypes>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <dlfcn.h>
#include <fcntl.h>
#include <unistd.h>

#ifndef LLVM_EXPORT
#define LLVM_EXPORT 0
#endif
#ifndef GCC_EXPORT
#define GCC_EXPORT 0
#endif
#ifndef LLVM_MISSING_REMOVE
#define LLVM_MISSING_REMOVE 0
#endif
#if (LLVM_EXPORT + GCC_EXPORT) != 1
#error "Select exactly one unwind probe ABI"
#endif
#if !defined(__linux__)
#error "The unwind preload probe requires Linux"
#endif
#if LLVM_MISSING_REMOVE && !LLVM_EXPORT
#error "Missing-remove mode requires the LLVM probe ABI"
#endif

#if GCC_EXPORT || LLVM_MISSING_REMOVE
using UnwindHook = void (*)(uintptr_t);
extern "C" __attribute__((visibility("hidden")))
UnwindHook tpde_mapper_null_unwind_hook() {
  return nullptr;
}
#endif

// Null IFUNCs control absence even when a linked library exports LLVM hooks.
namespace {
#if LLVM_EXPORT
constexpr const char *Family = "llvm";
#else
constexpr const char *Family = "gcc";
#endif
std::atomic_flag event_lock = ATOMIC_FLAG_INIT;
uintptr_t active_frames[4096] = {};

bool armed() {
  const char *value = std::getenv("TPDE_MAPPER_PROBE_ARMED");
  return value && std::strcmp(value, "1") == 0;
}

bool write_all(int fd, const char *data, size_t length) {
  while (length) {
    const auto count = ::write(fd, data, length);
    if (count < 0 && errno == EINTR) continue;
    if (count <= 0) return false;
    data += count;
    length -= size_t(count);
  }
  return true;
}

[[noreturn]] void fail(const char *reason, uintptr_t address, uint32_t record) {
  char line[192];
  const int length = std::snprintf(line, sizeof(line),
      "ERROR\t%s\t%s\t0x%" PRIxPTR "\t%" PRIu32 "\n",
      Family, reason, address, record);
  if (length > 0 && size_t(length) < sizeof(line)) {
    (void)write_all(STDERR_FILENO, line, size_t(length));
    if (const char *path = std::getenv("TPDE_MAPPER_PROBE_LOG")) {
      const int fd = ::open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
      if (fd >= 0) {
        (void)write_all(fd, line, size_t(length));
        ::close(fd);
      }
    }
  }
  ::_exit(93);
}

void event(const char *operation, uintptr_t address, uint32_t record) {
  const char *path = std::getenv("TPDE_MAPPER_PROBE_LOG");
  if (!path || !*path) fail("missing-log-path", address, record);
  char line[128];
  const int length = std::snprintf(line, sizeof(line),
      "%s-%s\t0x%" PRIxPTR "\t%" PRIu32 "\n",
      Family, operation, address, record);
  if (length <= 0 || size_t(length) >= sizeof(line))
    fail("log-format", address, record);
  const int fd = ::open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
  if (fd < 0) fail("log-open", address, record);
  const bool written = write_all(fd, line, size_t(length));
  ::close(fd);
  if (!written) fail("log-write", address, record);
}

uint32_t record_at(uintptr_t address) {
  if (!address) fail("null-frame", address, 0);
  const auto *data = reinterpret_cast<const volatile unsigned char *>(address);
  uint32_t value = 0;
  for (unsigned i = 0; i < 4; ++i) value |= uint32_t(data[i]) << (i * 8);
  if (value) fail("nonzero-frame-record", address, value);
  return value;
}

void register_frame(uintptr_t address) {
  while (event_lock.test_and_set(std::memory_order_acquire)) {}
  uintptr_t *empty = nullptr;
  for (auto &entry : active_frames) {
    if (entry && entry == address) fail("duplicate-register", address, 0);
    if (!entry && !empty) empty = &entry;
  }
  if (!empty) fail("active-frame-capacity", address, 0);
  const uint32_t record = record_at(address);
  *empty = address;
  event("register", address, record);
  event_lock.clear(std::memory_order_release);
}

void deregister_frame(uintptr_t address) {
  while (event_lock.test_and_set(std::memory_order_acquire)) {}
  uintptr_t *matched = nullptr;
  for (auto &entry : active_frames) {
    if (entry && entry == address) {
      matched = &entry;
      break;
    }
  }
  if (!matched) fail("unmatched-deregister", address, 0);
  const uint32_t record = record_at(address);
  event("deregister", address, record);
  *matched = 0;
  event_lock.clear(std::memory_order_release);
}

#if GCC_EXPORT
void forward(const char *name, void *address) {
  using Callback = void (*)(void *);
  const auto callback = reinterpret_cast<Callback>(::dlsym(RTLD_NEXT, name));
  if (!callback) fail("missing-next-unwind-hook", uintptr_t(address), 0);
  callback(address);
}
#endif
} // namespace

#if LLVM_EXPORT
extern "C" void __unw_add_dynamic_eh_frame_section(uintptr_t address) {
  if (armed()) register_frame(address);
}
#if LLVM_MISSING_REMOVE
extern "C" void __unw_remove_dynamic_eh_frame_section(uintptr_t)
    __attribute__((ifunc("tpde_mapper_null_unwind_hook")));
#else
extern "C" void __unw_remove_dynamic_eh_frame_section(uintptr_t address) {
  if (armed()) deregister_frame(address);
}
#endif
#else
extern "C" void __unw_add_dynamic_eh_frame_section(uintptr_t)
    __attribute__((ifunc("tpde_mapper_null_unwind_hook")));
extern "C" void __register_frame(void *address) {
  if (armed()) register_frame(reinterpret_cast<uintptr_t>(address));
  else forward("__register_frame", address);
}
extern "C" void __deregister_frame(void *address) {
  if (armed()) deregister_frame(reinterpret_cast<uintptr_t>(address));
  else forward("__deregister_frame", address);
}
#endif
