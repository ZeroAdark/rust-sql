// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

#ifndef TPDE_BASE_LOGGING
#define TPDE_BASE_LOGGING 1
#endif
#if TPDE_BASE_LOGGING
#define TPDE_LOGGING
#define SPDLOG_HEADER_ONLY
#define SPDLOG_USE_STD_FORMAT
#define SPDLOG_NO_EXCEPTIONS
#include <spdlog/sinks/ostream_sink.h>
#endif
#include "upstream/tpde/src/base.cpp"
#include <iostream>
#include <string>
#include <type_traits>

static void configure() {
#ifdef TPDE_LOGGING
  auto sink = std::make_shared<spdlog::sinks::ostream_sink_mt>(std::cout, true);
  auto logger = std::make_shared<spdlog::logger>("oracle", sink);
  logger->set_pattern("%v");
  spdlog::set_default_logger(logger);
  spdlog::set_level(spdlog::level::trace);
#endif
}

template <typename T> static void scalar() {
  std::cout << sizeof(T) << ':' << alignof(T) << ':' << std::is_signed_v<T> << '\n';
}

static void normal() {
  scalar<tpde::u8>();
  scalar<tpde::u16>();
  scalar<tpde::u32>();
  scalar<tpde::u64>();
  scalar<tpde::i8>();
  scalar<tpde::i16>();
  scalar<tpde::i32>();
  scalar<tpde::i64>();
  std::cout << "asserts:" << tpde::WithAsserts << '\n';
  for (unsigned threshold = 0; threshold != 7; ++threshold) {
#ifdef TPDE_LOGGING
    spdlog::set_level(static_cast<spdlog::level::level_enum>(threshold));
#endif
    unsigned evaluations = 0;
    std::cout << "threshold:" << threshold << '\n';
    TPDE_LOG_TRACE("trace:{}", ++evaluations);
    TPDE_LOG_DBG("debug:{}", ++evaluations);
    TPDE_LOG_INFO("info:{}", ++evaluations);
    TPDE_LOG_WARN("warn:{}", ++evaluations);
    TPDE_LOG_ERR("error:{}", ++evaluations);
#ifdef TPDE_LOGGING
    TPDE_LOG(spdlog::level::critical, "critical:{}", ++evaluations);
    TPDE_LOG(spdlog::level::off, "off:{}", ++evaluations);
#endif
    std::cout << "evaluations:" << evaluations << '\n';
  }
  unsigned levels = 0;
  unsigned values = 0;
#ifdef TPDE_LOGGING
  spdlog::set_level(spdlog::level::info);
  TPDE_LOG((++levels, spdlog::level::info), "generic:{}", ++values);
  TPDE_LOG((++levels, spdlog::level::trace), "hidden:{}", ++values);
  TPDE_LOG_INFO("mutate:{}", (spdlog::set_level(spdlog::level::off), ++values));
#endif
  std::cout << "generic-evaluations:" << levels << ':' << values << '\n';
#if !defined(TPDE_LOGGING) || defined(NDEBUG)
  TPDE_LOG_TRACE(undefined_name_that_must_not_be_typechecked);
  TPDE_LOG_DBG(undefined_name_that_must_not_be_typechecked);
#endif
#ifndef TPDE_LOGGING
  TPDE_LOG_INFO(undefined_name_that_must_not_be_typechecked);
  TPDE_LOG_WARN(undefined_name_that_must_not_be_typechecked);
  TPDE_LOG_ERR(undefined_name_that_must_not_be_typechecked);
#endif
  unsigned a = 7, b = 8;
  TPDE_NOALIAS(&a, &b);
  a += 3;
  b += 5;
  std::cout << "separate:" << a << ':' << b << '\n';
}

int main(int argc, char **argv) {
  configure();
  std::string mode = argc > 1 ? argv[1] : "normal";
  if (mode == "normal") {
    normal();
    return 0;
  }
  if (mode == "unreachable") {
    TPDE_UNREACHABLE("unreachable detail");
    return 1;
  }
  if (mode == "fatal-off") {
#ifdef TPDE_LOGGING
    spdlog::set_level(spdlog::level::off);
#endif
  }
#ifdef __cpp_exceptions
  try {
    TPDE_FATAL("fatal detail");
  } catch (const std::runtime_error &error) {
    std::cout << "caught:" << error.what() << '\n';
    return 0;
  }
#else
  TPDE_FATAL("fatal detail");
#endif
}
