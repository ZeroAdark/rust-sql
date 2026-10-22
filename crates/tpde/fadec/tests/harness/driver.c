// SPDX-FileCopyrightText: 2018 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause

#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "encode2.c"

typedef struct Case {
    unsigned flags;
    uint8_t regs[4], mask;
    int64_t imm;
    uintptr_t absolute;
    int64_t delta;
    uint8_t memflags, base, scale, index;
    int32_t off;
    unsigned opcode, ripoff, forcesib, disp8scale, size;
} Case;

static const Case cases[] = {CASE_VALUES};
static const unsigned selected[] = {SELECTION_VALUES};
static const unsigned offsets[] = {OFFSET_VALUES};
static const unsigned helper_cases[][2] = {HELPER_VALUES};
enum { function_count = FUNCTION_COUNT };

static FeRegGPLH raw_gplh(uint8_t idx) {
#ifdef __cplusplus
    return FeRegGPLH(FeRegGP{idx});
#else
    return (FeRegGPLH){idx};
#endif
}

#ifdef __cplusplus
CXX_GROUP_FUNCTIONS

static unsigned invoke(unsigned function, uint8_t *buf, const Case *c) {
    switch (function / CXX_GROUP_SIZE) {
        CXX_GROUP_DISPATCH
        default: abort();
    }
}
#else
static unsigned invoke(unsigned function, uint8_t *buf, const Case *c) {
    switch (function) {
        FORM_CALLS
        default: abort();
    }
}
#endif

static unsigned helper(unsigned function, uint8_t *buf, const Case *c) {
    FeMem mem = {c->memflags, {c->base}, c->scale, {c->index}, c->off};
    FeMemV memv = {c->memflags, {c->base}, c->scale, {c->index}, c->off};
    unsigned reg = c->regs[0], vvvv = c->regs[1];
    switch (function) {
        case 0: return op_imm_n(c->imm, c->size);
        case 1: return enc_seg67(buf, c->flags);
        case 2: return enc_rex_mem(mem, reg);
        case 3: enc_imm(buf, c->imm, c->size); return c->size;
        case 4: return enc_mem(buf, c->ripoff, mem, reg, c->forcesib, c->disp8scale);
        case 5: return enc_vex_common(buf, c->opcode, c->base, c->index, reg, vvvv);
        case 6: return enc_vex_reg(buf, c->opcode, c->base, reg, vvvv);
        case 7: return enc_evex_reg(buf, c->opcode, c->base, reg, vvvv);
        case 8: return enc_evex_xmm(buf, c->opcode, c->base, reg, vvvv);
        case 9: return enc_vex_mem(buf, c->opcode, mem, reg, vvvv, c->ripoff, c->forcesib, c->disp8scale);
        case 10: return enc_evex_mem(buf, c->opcode, mem, reg, vvvv, c->ripoff, c->forcesib, c->disp8scale);
        case 11: return enc_vex_vsib(buf, c->opcode, memv, reg, vvvv, c->ripoff, c->forcesib, c->disp8scale);
        case 12: return enc_evex_vsib(buf, c->opcode, memv, reg, vvvv, c->ripoff, c->forcesib, c->disp8scale);
        default: abort();
    }
}

static void record(unsigned function, unsigned caseid, unsigned result, const uint8_t *buf, unsigned length) {
    printf("%u\t%u\t%u\t", function, caseid, result);
    for (unsigned i = 0; i < length; ++i) printf("%02x", buf[i]);
    putchar('\n');
}

static void canaries(const uint8_t *storage) {
    for (unsigned i = 0; i < 16; ++i)
        assert(storage[i] == 0x5a && storage[48+i] == 0x5a);
}

static void forms(void) {
    assert(sizeof offsets / sizeof offsets[0] == function_count + 1);
    for (unsigned function = 0; function < function_count; ++function) {
        assert(offsets[function] < offsets[function+1]);
        for (unsigned run = offsets[function]; run < offsets[function+1]; ++run) {
            unsigned caseid = selected[run];
            uint8_t storage[64];
            memset(storage, 0x5a, sizeof storage);
            uint8_t *buf = storage + 16;
            memset(buf, 0xa5, 32);
            assert((uintptr_t)buf >= UINT64_C(0x100000000));
            assert((uintptr_t)buf < UINT64_C(0x7fffffff00000000));
            unsigned result = invoke(function, buf, &cases[caseid]);
            assert(result <= 32);
            canaries(storage);
            record(function, caseid, result, buf, result);
        }
    }
}

static void helpers(void) {
    for (unsigned run = 0; run < sizeof helper_cases / sizeof helper_cases[0]; ++run) {
        unsigned function = helper_cases[run][0], caseid = helper_cases[run][1];
        uint8_t storage[64];
        memset(storage, 0x5a, sizeof storage);
        uint8_t *buf = storage + 16;
        memset(buf, 0xa5, 32);
        unsigned result = helper(function, buf, &cases[caseid]);
        canaries(storage);
        record(function, caseid, result, buf, 32);
    }
}

static void nop(unsigned id, unsigned length) {
    unsigned size = length ? length : 1;
    uint8_t *storage = (uint8_t*)malloc(size + 32);
    assert(storage);
    memset(storage, 0x5a, size + 32);
    uint8_t *buf = storage + 16;
    memset(buf, 0xa5, size);
    unsigned result = fe64_NOP(buf, length);
    assert(result == size);
    for (unsigned i = 0; i < 16; ++i)
        assert(storage[i] == 0x5a && storage[size+16+i] == 0x5a);
    record(id, length, result, buf, result);
    free(storage);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    setvbuf(stdout, NULL, _IOFBF, 1024 * 1024);
    if (!strcmp(argv[1], "forms")) forms();
    else if (!strcmp(argv[1], "helpers")) helpers();
    else if (!strcmp(argv[1], "nops")) {
        for (unsigned length = 0; length <= 128; ++length) nop(length, length);
        const unsigned sizes[] = {255, 256, 511, 512, 1023, 1024, 4095, 4096, 65535, 65536};
        for (unsigned i = 0; i < sizeof sizes / sizeof sizes[0]; ++i) nop(129+i, sizes[i]);
    } else abort();
    assert(fflush(stdout) == 0);
}
