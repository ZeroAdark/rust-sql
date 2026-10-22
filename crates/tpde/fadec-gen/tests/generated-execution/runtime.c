// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#include <assert.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define FE_JMPL 0x8
#define FE_ADDR32 0x10
#define FE_SEG_MASK 0x7
#define FE_CC_MASK 0xf0000
#define FE_RC_MASK 0x60
#define UNLIKELY(value) (value)
#define op_reg_idx(op) ((op).idx)
#define op_reg_gph(op) (((op).idx & ~0x3) == 0x24)

typedef struct FeRegGP { uint8_t idx; } FeRegGP;
typedef struct FeRegGPH { uint8_t idx; } FeRegGPH;
typedef struct FeRegGPLH { uint8_t idx; } FeRegGPLH;
typedef struct FeRegSREG { uint8_t idx; } FeRegSREG;
typedef struct FeRegST { uint8_t idx; } FeRegST;
typedef struct FeRegMM { uint8_t idx; } FeRegMM;
typedef struct FeRegXMM { uint8_t idx; } FeRegXMM;
typedef struct FeRegMASK { uint8_t idx; } FeRegMASK;
typedef struct FeRegTMM { uint8_t idx; } FeRegTMM;
typedef struct FeRegCR { uint8_t idx; } FeRegCR;
typedef struct FeRegDR { uint8_t idx; } FeRegDR;

typedef struct FeMem {
    uint8_t flags;
    FeRegGP base;
    uint8_t scale;
    FeRegGP idx;
    int32_t off;
} FeMem;

typedef struct FeMemV {
    uint8_t flags;
    FeRegGP base;
    uint8_t scale;
    FeRegXMM idx;
    int32_t off;
} FeMemV;

static uint8_t *recording_base;
static unsigned recording_failure;
static unsigned recording_variant;
static char recording_trace[32768];
static size_t recording_trace_len;

static void recording_reset(uint8_t *base, unsigned failure, unsigned variant) {
    assert(base != NULL);
    assert(variant <= 1);
    recording_base = base;
    recording_failure = failure;
    recording_variant = variant;
    recording_trace_len = 0;
    recording_trace[0] = '\0';
}

static ptrdiff_t recording_offset(const uint8_t *buf) {
    if (buf == NULL) return -1;
    assert(recording_base != NULL);
    assert((uintptr_t) buf >= (uintptr_t) recording_base);
    uintptr_t offset = (uintptr_t) buf - (uintptr_t) recording_base;
    assert(offset <= 32);
    return (ptrdiff_t) offset;
}

static void recording_append(const char *format, ...) {
    va_list args;
    va_start(args, format);
    size_t available = sizeof recording_trace - recording_trace_len;
    int written = vsnprintf(recording_trace + recording_trace_len, available, format, args);
    va_end(args);
    assert(written >= 0 && (size_t) written < available);
    recording_trace_len += (size_t) written;
}

static void recording_log(unsigned id, const uint8_t *buf,
                          const uint64_t *args, size_t count, uint64_t result) {
    recording_append("%u,%td", id, recording_offset(buf));
    for (size_t i = 0; i < count; ++i)
        recording_append(",%" PRIu64, args[i]);
    recording_append(",%" PRIu64 ";", result);
}

static unsigned recording_marker(unsigned id, uint8_t *buf, unsigned ripoff) {
    unsigned length = recording_variant ? 5 : 3;
    assert((size_t) recording_offset(buf) + length <= 32);
    for (unsigned i = 0; i < length; ++i)
        buf[i] = (uint8_t) (id * 17 + i * 29);
    if (recording_failure == id) return 0;
    assert(ripoff <= 32 - length);
    return ripoff + length;
}

static void recording_finish(unsigned functionid, unsigned caseid, unsigned result) {
    assert(result <= 32);
    static const char hex[] = "0123456789abcdef";
    char row[sizeof recording_trace + 128];
    int prefix = snprintf(row, sizeof row, "%u\t%u\t%u\t", functionid, caseid, result);
    assert(prefix >= 0 && (size_t) prefix < sizeof row);
    size_t pos = (size_t) prefix;
    for (size_t i = 0; i < 32; ++i) {
        row[pos++] = hex[recording_base[i] >> 4];
        row[pos++] = hex[recording_base[i] & 0xf];
    }
    row[pos++] = '\t';
    assert(pos + recording_trace_len + 2 <= sizeof row);
    memcpy(row + pos, recording_trace, recording_trace_len);
    pos += recording_trace_len;
    row[pos++] = '\n';
    row[pos] = '\0';
    int status = fputs(row, stdout);
    assert(status >= 0);
}

#define RECORDING_MEMORY(mem) \
    (uint64_t) (mem).flags, (uint64_t) (mem).base.idx, \
    (uint64_t) (mem).scale, (uint64_t) (mem).idx.idx, (uint64_t) (int64_t) (mem).off
#define RECORDING_LOG(id, buf, args, result) \
    recording_log(id, buf, args, sizeof(args) / sizeof((args)[0]), result)

static bool op_imm_n(int64_t imm, unsigned immsz) {
    bool result = (immsz == 0 && imm == 0)
        || (immsz == 1 && imm >= INT8_MIN && imm <= INT8_MAX)
        || (immsz == 2 && imm >= INT16_MIN && imm <= INT16_MAX)
        || (immsz == 3 && imm >= 0 && imm <= 0xffffff)
        || (immsz == 4 && imm >= INT32_MIN && imm <= INT32_MAX)
        || immsz == 8;
    uint64_t args[] = { (uint64_t) imm, immsz };
    RECORDING_LOG(1, NULL, args, result);
    return result;
}

static unsigned enc_seg67(uint8_t *buf, unsigned flags) {
    static const uint8_t segments[] = { 0, 0x26, 0x2e, 0x36, 0x3e, 0x64, 0x65, 0 };
    unsigned segment = flags & FE_SEG_MASK;
    assert(segment < 7);
    unsigned length = (segment != 0) + ((flags & FE_ADDR32) != 0);
    assert((size_t) recording_offset(buf) + length <= 32);
    unsigned pos = 0;
    if (segment) buf[pos++] = segments[segment];
    if (flags & FE_ADDR32) buf[pos++] = 0x67;
    uint64_t args[] = { flags };
    RECORDING_LOG(2, buf, args, length);
    return length;
}

static unsigned enc_rex_mem(FeMem mem, uint64_t reg) {
    uint32_t val = (uint32_t) (reg | mem.flags | ((uint32_t) mem.base.idx << 8)
                              | ((uint32_t) mem.idx.idx << 24));
    uint32_t masked = val & UINT32_C(0x08000808);
    uint32_t product = (uint32_t) (masked * UINT32_C(0x02008001));
    unsigned result = masked ? (uint8_t) (product >> 26) + 0x40 : 0;
    uint64_t args[] = { RECORDING_MEMORY(mem), reg };
    RECORDING_LOG(3, NULL, args, result);
    return result;
}

static void enc_imm(uint8_t *buf, uint64_t imm, unsigned immsz) {
    assert(immsz <= 8);
    assert((size_t) recording_offset(buf) + immsz <= 32);
    for (unsigned i = 0; i < immsz; ++i)
        buf[i] = (uint8_t) (imm >> (8 * i));
    uint64_t args[] = { imm, immsz };
    RECORDING_LOG(4, buf, args, 0);
}

static int enc_mem(uint8_t *buf, unsigned ripoff, FeMem mem, uint64_t reg,
                   bool forcesib, unsigned disp8scale) {
    unsigned result = recording_marker(5, buf, ripoff);
    uint64_t args[] = { ripoff, RECORDING_MEMORY(mem), reg, forcesib, disp8scale };
    RECORDING_LOG(5, buf, args, result);
    return (int) result;
}

static int enc_vex_common(uint8_t *buf, unsigned opcode, unsigned base,
                          unsigned idx, unsigned reg, unsigned vvvv) {
    unsigned result = recording_marker(6, buf, 0);
    uint64_t args[] = { opcode, base, idx, reg, vvvv };
    RECORDING_LOG(6, buf, args, result);
    return (int) result;
}

static int enc_vex_reg(uint8_t *buf, unsigned opcode, uint64_t rm,
                       uint64_t reg, uint64_t vvvv) {
    unsigned result = recording_marker(7, buf, 0);
    uint64_t args[] = { opcode, rm, reg, vvvv };
    RECORDING_LOG(7, buf, args, result);
    return (int) result;
}

static int enc_evex_reg(uint8_t *buf, unsigned opcode, unsigned rm,
                        unsigned reg, unsigned vvvv) {
    unsigned result = recording_marker(8, buf, 0);
    uint64_t args[] = { opcode, rm, reg, vvvv };
    RECORDING_LOG(8, buf, args, result);
    return (int) result;
}

static int enc_evex_xmm(uint8_t *buf, unsigned opcode, unsigned rm,
                        unsigned reg, unsigned vvvv) {
    unsigned result = recording_marker(9, buf, 0);
    uint64_t args[] = { opcode, rm, reg, vvvv };
    RECORDING_LOG(9, buf, args, result);
    return (int) result;
}

static int enc_vex_mem(uint8_t *buf, unsigned opcode, FeMem rm, uint64_t reg,
                       uint64_t vvvv, unsigned ripoff, bool forcesib,
                       unsigned disp8scale) {
    unsigned result = recording_marker(10, buf, ripoff);
    uint64_t args[] = { opcode, RECORDING_MEMORY(rm), reg, vvvv, ripoff, forcesib, disp8scale };
    RECORDING_LOG(10, buf, args, result);
    return (int) result;
}

static int enc_evex_mem(uint8_t *buf, unsigned opcode, FeMem rm, uint64_t reg,
                        uint64_t vvvv, unsigned ripoff, bool forcesib,
                        unsigned disp8scale) {
    unsigned result = recording_marker(11, buf, ripoff);
    uint64_t args[] = { opcode, RECORDING_MEMORY(rm), reg, vvvv, ripoff, forcesib, disp8scale };
    RECORDING_LOG(11, buf, args, result);
    return (int) result;
}

static int enc_vex_vsib(uint8_t *buf, unsigned opcode, FeMemV rm, uint64_t reg,
                        uint64_t vvvv, unsigned ripoff, bool forcesib,
                        unsigned disp8scale) {
    unsigned result = recording_marker(12, buf, ripoff);
    uint64_t args[] = { opcode, RECORDING_MEMORY(rm), reg, vvvv, ripoff, forcesib, disp8scale };
    RECORDING_LOG(12, buf, args, result);
    return (int) result;
}

static int enc_evex_vsib(uint8_t *buf, unsigned opcode, FeMemV rm, uint64_t reg,
                         uint64_t vvvv, unsigned ripoff, bool forcesib,
                         unsigned disp8scale) {
    unsigned result = recording_marker(13, buf, ripoff);
    uint64_t args[] = { opcode, RECORDING_MEMORY(rm), reg, vvvv, ripoff, forcesib, disp8scale };
    RECORDING_LOG(13, buf, args, result);
    return (int) result;
}
