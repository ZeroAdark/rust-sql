# gravity-asm corpus: kernel par_scan_count
# entry: gravity_kernels::par_scan_count::par_scan_count
# symbol: gravity_kernels::par_scan_count::par_scan_count
# arch arm64 Darwin; rustc 1.96.0 (ac68faa20 2026-05-25); profile dist (lto fat, codegen-units 1); 2026-09-16T15:49Z; af7d08b382e
# function: 755 instructions, 134 blocks, 22 loops, 20 innermost: 18 hot, 2 window, 0 other
# hot LBB97_16: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_37: 16 instructions in 4 blocks, loads 3 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_44: 16 instructions in 4 blocks, loads 3 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_51: 16 instructions in 4 blocks, loads 3 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_77: 20 instructions in 1 blocks, loads 3 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_80: 11 instructions in 1 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_82: 20 instructions in 1 blocks, loads 4 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_85: 11 instructions in 1 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_87: 20 instructions in 1 blocks, loads 3 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_90: 11 instructions in 1 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_96: 25 instructions in 1 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_99: 5 instructions in 1 blocks, loads 1 (gather 0), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_101: 33 instructions in 1 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_105: 11 instructions in 1 blocks, loads 1 (gather 0), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_108: 5 instructions in 1 blocks, loads 1 (gather 0), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_110: 72 instructions in 1 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 3
# hot LBB97_114: 11 instructions in 1 blocks, loads 1 (gather 0), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB97_117: 5 instructions in 1 blocks, loads 1 (gather 0), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# window LBB97_5: 16 instructions in 5 blocks, loads 7 (gather 1), stores 0, tests 2, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
# window LBB97_21: 16 instructions in 5 blocks, loads 7 (gather 1), stores 0, tests 2, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
## hot LBB97_16 (blocks [18])
LBB97_16:
	rbit	w11, w8                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x9, w11, uxtw]                ; gather
	str	x10, [x23, x11]                      ; store
	sub	w11, w8, #1                          ; alu
	and	w8, w11, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB97_16                            ; branch
## hot LBB97_37 (blocks [39, 40, 41, 42])
LBB97_36:
	ldr	w13, [x9, x13, lsl #2]               ; gather
	cmp	w13, w22                             ; test
	cset	w13, lt                             ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.eq	LBB97_29                            ; branch
LBB97_37:
	ldr	w13, [x12], #4                       ; load
	cbz	x11, LBB97_36                        ; branch
	lsr	x14, x13, #6                         ; alu
	ldr	x14, [x11, x14, lsl #3]              ; gather
	lsr	x14, x14, x13                        ; alu
	tbnz	w14, #0, LBB97_36                   ; branch
	mov	x13, #0                              ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.ne	LBB97_37                            ; branch
## hot LBB97_44 (blocks [47, 48, 49, 50])
LBB97_43:
	ldrsh	w13, [x9, x13, lsl #1]             ; gather
	cmp	w13, w22                             ; test
	cset	w13, lt                             ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.eq	LBB97_29                            ; branch
LBB97_44:
	ldr	w13, [x12], #4                       ; load
	cbz	x11, LBB97_43                        ; branch
	lsr	x14, x13, #6                         ; alu
	ldr	x14, [x11, x14, lsl #3]              ; gather
	lsr	x14, x14, x13                        ; alu
	tbnz	w14, #0, LBB97_43                   ; branch
	mov	x13, #0                              ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.ne	LBB97_44                            ; branch
## hot LBB97_51 (blocks [55, 56, 57, 58])
LBB97_50:
	ldr	x13, [x9, x13, lsl #3]               ; gather
	cmp	w13, w22                             ; test
	cset	w13, lt                             ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.eq	LBB97_29                            ; branch
LBB97_51:
	ldr	w13, [x12], #4                       ; load
	cbz	x11, LBB97_50                        ; branch
	lsr	x14, x13, #6                         ; alu
	ldr	x14, [x11, x14, lsl #3]              ; gather
	lsr	x14, x14, x13                        ; alu
	tbnz	w14, #0, LBB97_50                   ; branch
	mov	x13, #0                              ; alu
	add	x8, x13, x8                          ; alu
	subs	x10, x10, #1                        ; alu
	b.ne	LBB97_51                            ; branch
## hot LBB97_77 (blocks [83])
LBB97_77:
	ldp	w0, w1, [x11, #-4]                   ; load
	sub	x2, x16, #1                          ; alu
	lsr	x3, x2, #6                           ; alu
	lsr	x4, x16, #6                          ; alu
	ldr	x3, [x13, x3, lsl #3]                ; gather
	ldr	x4, [x13, x4, lsl #3]                ; gather
	lsr	x2, x3, x2                           ; alu
	lsr	x3, x4, x16                          ; alu
	cmp	w0, w22                              ; test
	cset	w0, lt                              ; alu
	cmp	w1, w22                              ; test
	cset	w1, lt                              ; alu
	and	w0, w2, w0                           ; alu
	and	w1, w3, w1                           ; alu
	add	x8, x8, x0                           ; alu
	add	x15, x15, x1                         ; alu
	add	x16, x16, #2                         ; alu
	add	x11, x11, #8                         ; alu
	subs	x17, x17, #2                        ; alu
	b.ne	LBB97_77                            ; branch
## hot LBB97_80 (blocks [86])
LBB97_80:
	ldr	w12, [x9, x10, lsl #2]               ; gather
	lsr	x14, x10, #6                         ; alu
	ldr	x14, [x13, x14, lsl #3]              ; gather
	lsr	x14, x14, x10                        ; alu
	cmp	w12, w22                             ; test
	cset	w12, lt                             ; alu
	and	w12, w14, w12                        ; alu
	add	x8, x8, x12                          ; alu
	add	x10, x10, #1                         ; alu
	subs	x11, x11, #1                        ; alu
	b.ne	LBB97_80                            ; branch
## hot LBB97_82 (blocks [89])
LBB97_82:
	ldurh	w0, [x11, #-2]                     ; load
	ldrh	w1, [x11], #4                       ; load
	sub	x2, x16, #1                          ; alu
	lsr	x3, x2, #6                           ; alu
	lsr	x4, x16, #6                          ; alu
	ldr	x3, [x13, x3, lsl #3]                ; gather
	ldr	x4, [x13, x4, lsl #3]                ; gather
	lsr	x2, x3, x2                           ; alu
	lsr	x3, x4, x16                          ; alu
	cmp	w22, w0, sxth                        ; test
	cset	w0, gt                              ; alu
	cmp	w22, w1, sxth                        ; test
	cset	w1, gt                              ; alu
	and	w0, w2, w0                           ; alu
	and	w1, w3, w1                           ; alu
	add	x8, x8, x0                           ; alu
	add	x15, x15, x1                         ; alu
	add	x16, x16, #2                         ; alu
	subs	x17, x17, #2                        ; alu
	b.ne	LBB97_82                            ; branch
## hot LBB97_85 (blocks [92])
LBB97_85:
	ldrh	w12, [x9, x10, lsl #1]              ; gather
	lsr	x14, x10, #6                         ; alu
	ldr	x14, [x13, x14, lsl #3]              ; gather
	lsr	x14, x14, x10                        ; alu
	cmp	w22, w12, sxth                       ; test
	cset	w12, gt                             ; alu
	and	w12, w14, w12                        ; alu
	add	x8, x8, x12                          ; alu
	add	x10, x10, #1                         ; alu
	subs	x11, x11, #1                        ; alu
	b.ne	LBB97_85                            ; branch
## hot LBB97_87 (blocks [95])
LBB97_87:
	ldp	x0, x1, [x12, #-8]                   ; load
	sub	x2, x16, #1                          ; alu
	lsr	x3, x2, #6                           ; alu
	lsr	x4, x16, #6                          ; alu
	ldr	x3, [x11, x3, lsl #3]                ; gather
	ldr	x4, [x11, x4, lsl #3]                ; gather
	lsr	x2, x3, x2                           ; alu
	lsr	x3, x4, x16                          ; alu
	cmp	w22, w0                              ; test
	cset	w0, gt                              ; alu
	cmp	w22, w1                              ; test
	cset	w1, gt                              ; alu
	and	w0, w2, w0                           ; alu
	and	w1, w3, w1                           ; alu
	add	x8, x8, x0                           ; alu
	add	x15, x15, x1                         ; alu
	add	x16, x16, #2                         ; alu
	add	x12, x12, #16                        ; alu
	subs	x17, x17, #2                        ; alu
	b.ne	LBB97_87                            ; branch
## hot LBB97_90 (blocks [98])
LBB97_90:
	ldr	x13, [x9, x10, lsl #3]               ; gather
	lsr	x14, x10, #6                         ; alu
	ldr	x14, [x11, x14, lsl #3]              ; gather
	lsr	x14, x14, x10                        ; alu
	cmp	w22, w13                             ; test
	cset	w13, gt                             ; alu
	and	w13, w14, w13                        ; alu
	add	x8, x8, x13                          ; alu
	add	x10, x10, #1                         ; alu
	subs	x12, x12, #1                        ; alu
	b.ne	LBB97_90                            ; branch
## hot LBB97_96 (blocks [105])
LBB97_96:
	ldp	q4, q5, [x8, #-32]                   ; load
	ldp	q6, q7, [x8], #64                    ; load
	xtn.2s	v4, v4                            ; alu
	xtn.2s	v5, v5                            ; alu
	xtn.2s	v6, v6                            ; alu
	xtn.2s	v7, v7                            ; alu
	cmgt.2s	v4, v25, v4                      ; alu
	ushll.2d	v4, v4, #0                      ; alu
	dup.2d	v16, x26                          ; alu
	and.16b	v4, v4, v16                      ; alu
	cmgt.2s	v5, v25, v5                      ; alu
	ushll.2d	v5, v5, #0                      ; alu
	and.16b	v5, v5, v16                      ; alu
	cmgt.2s	v6, v25, v6                      ; alu
	ushll.2d	v6, v6, #0                      ; alu
	and.16b	v6, v6, v16                      ; alu
	cmgt.2s	v7, v25, v7                      ; alu
	ushll.2d	v7, v7, #0                      ; alu
	and.16b	v7, v7, v16                      ; alu
	add.2d	v0, v0, v4                        ; alu
	add.2d	v1, v1, v5                        ; alu
	add.2d	v2, v2, v6                        ; alu
	add.2d	v3, v3, v7                        ; alu
	subs	x15, x15, #8                        ; alu
	b.ne	LBB97_96                            ; branch
## hot LBB97_99 (blocks [108])
LBB97_99:
	ldr	w11, [x9], #8                        ; load
	cmp	w22, w11                             ; test
	cinc	x8, x8, gt                          ; alu
	adds	x10, x10, #1                        ; alu
	b.lo	LBB97_99                            ; branch
## hot LBB97_101 (blocks [111])
LBB97_101:
	ldp	q16, q17, [x8, #-32]                 ; load
	ldp	q18, q19, [x8], #64                  ; load
	cmgt.4s	v16, v25, v16                    ; alu
	ushll.2d	v20, v16, #0                    ; alu
	dup.2d	v21, x26                          ; alu
	and.16b	v20, v20, v21                    ; alu
	ushll2.2d	v16, v16, #0                   ; alu
	and.16b	v16, v16, v21                    ; alu
	cmgt.4s	v17, v25, v17                    ; alu
	ushll.2d	v22, v17, #0                    ; alu
	and.16b	v22, v22, v21                    ; alu
	ushll2.2d	v17, v17, #0                   ; alu
	and.16b	v17, v17, v21                    ; alu
	cmgt.4s	v18, v25, v18                    ; alu
	ushll.2d	v23, v18, #0                    ; alu
	and.16b	v23, v23, v21                    ; alu
	ushll2.2d	v18, v18, #0                   ; alu
	and.16b	v18, v18, v21                    ; alu
	cmgt.4s	v19, v25, v19                    ; alu
	ushll.2d	v24, v19, #0                    ; alu
	and.16b	v24, v24, v21                    ; alu
	ushll2.2d	v19, v19, #0                   ; alu
	and.16b	v19, v19, v21                    ; alu
	add.2d	v2, v2, v16                       ; alu
	add.2d	v1, v1, v20                       ; alu
	add.2d	v5, v5, v17                       ; alu
	add.2d	v4, v4, v22                       ; alu
	add.2d	v7, v7, v18                       ; alu
	add.2d	v3, v3, v23                       ; alu
	add.2d	v0, v0, v19                       ; alu
	add.2d	v6, v6, v24                       ; alu
	subs	x16, x16, #16                       ; alu
	b.ne	LBB97_101                           ; branch
## hot LBB97_105 (blocks [115])
LBB97_105:
	ldr	q2, [x15], #16                       ; load
	cmgt.4s	v2, v25, v2                      ; alu
	ushll.2d	v3, v2, #0                      ; alu
	dup.2d	v4, x26                           ; alu
	and.16b	v3, v3, v4                       ; alu
	ushll2.2d	v2, v2, #0                     ; alu
	and.16b	v2, v2, v4                       ; alu
	add.2d	v0, v0, v2                        ; alu
	add.2d	v1, v1, v3                        ; alu
	adds	x8, x8, #4                          ; alu
	b.ne	LBB97_105                           ; branch
## hot LBB97_108 (blocks [118])
LBB97_108:
	ldr	w11, [x9], #4                        ; load
	cmp	w11, w22                             ; test
	cinc	x8, x8, lt                          ; alu
	adds	x10, x10, #1                        ; alu
	b.lo	LBB97_108                           ; branch
## hot LBB97_110 (blocks [121])
LBB97_110:
	str	q24, [sp, #16]                       ; STACK store
	ldp	q24, q25, [x8, #-32]                 ; load
	sshll2.4s	v26, v24, #0                   ; alu
	sshll.4s	v24, v24, #0                    ; alu
	sshll2.4s	v27, v25, #0                   ; alu
	sshll.4s	v25, v25, #0                    ; alu
	ldp	q28, q29, [x8], #64                  ; load
	sshll2.4s	v30, v28, #0                   ; alu
	sshll.4s	v28, v28, #0                    ; alu
	sshll2.4s	v31, v29, #0                   ; alu
	sshll.4s	v29, v29, #0                    ; alu
	cmgt.4s	v24, v0, v24                     ; alu
	ushll.2d	v8, v24, #0                     ; alu
	dup.2d	v9, x26                           ; alu
	and.16b	v8, v8, v9                       ; alu
	str	q8, [sp]                             ; STACK store
	ushll2.2d	v24, v24, #0                   ; alu
	and.16b	v24, v24, v9                     ; alu
	cmgt.4s	v26, v0, v26                     ; alu
	ushll.2d	v10, v26, #0                    ; alu
	and.16b	v10, v10, v9                     ; alu
	ushll2.2d	v26, v26, #0                   ; alu
	and.16b	v26, v26, v9                     ; alu
	cmgt.4s	v25, v0, v25                     ; alu
	ushll.2d	v11, v25, #0                    ; alu
	and.16b	v11, v11, v9                     ; alu
	ushll2.2d	v25, v25, #0                   ; alu
	and.16b	v25, v25, v9                     ; alu
	cmgt.4s	v27, v0, v27                     ; alu
	ushll.2d	v12, v27, #0                    ; alu
	and.16b	v12, v12, v9                     ; alu
	ushll2.2d	v27, v27, #0                   ; alu
	and.16b	v27, v27, v9                     ; alu
	cmgt.4s	v28, v0, v28                     ; alu
	ushll.2d	v13, v28, #0                    ; alu
	and.16b	v13, v13, v9                     ; alu
	ushll2.2d	v28, v28, #0                   ; alu
	and.16b	v28, v28, v9                     ; alu
	cmgt.4s	v30, v0, v30                     ; alu
	ushll.2d	v14, v30, #0                    ; alu
	and.16b	v14, v14, v9                     ; alu
	ushll2.2d	v30, v30, #0                   ; alu
	and.16b	v30, v30, v9                     ; alu
	cmgt.4s	v29, v0, v29                     ; alu
	ushll.2d	v15, v29, #0                    ; alu
	and.16b	v15, v15, v9                     ; alu
	ushll2.2d	v29, v29, #0                   ; alu
	and.16b	v29, v29, v9                     ; alu
	cmgt.4s	v31, v0, v31                     ; alu
	ushll.2d	v8, v31, #0                     ; alu
	and.16b	v8, v8, v9                       ; alu
	ushll2.2d	v31, v31, #0                   ; alu
	and.16b	v31, v31, v9                     ; alu
	add.2d	v4, v4, v26                       ; alu
	add.2d	v2, v2, v10                       ; alu
	add.2d	v3, v3, v24                       ; alu
	ldp	q26, q24, [sp]                       ; STACK load
	add.2d	v1, v1, v26                       ; alu
	add.2d	v21, v21, v27                     ; alu
	add.2d	v16, v16, v12                     ; alu
	add.2d	v19, v19, v25                     ; alu
	add.2d	v5, v5, v11                       ; alu
	add.2d	v20, v20, v30                     ; alu
	add.2d	v22, v22, v14                     ; alu
	add.2d	v6, v6, v28                       ; alu
	add.2d	v17, v17, v13                     ; alu
	add.2d	v23, v23, v31                     ; alu
	add.2d	v18, v18, v8                      ; alu
	add.2d	v24, v24, v29                     ; alu
	add.2d	v7, v7, v15                       ; alu
	subs	x16, x16, #32                       ; alu
	b.ne	LBB97_110                           ; branch
## hot LBB97_114 (blocks [125])
LBB97_114:
	ldr	d2, [x15], #8                        ; load
	sshll.4s	v2, v2, #0                      ; alu
	cmgt.4s	v2, v25, v2                      ; alu
	ushll.2d	v3, v2, #0                      ; alu
	and.16b	v3, v3, v4                       ; alu
	ushll2.2d	v2, v2, #0                     ; alu
	and.16b	v2, v2, v4                       ; alu
	add.2d	v0, v0, v2                        ; alu
	add.2d	v1, v1, v3                        ; alu
	adds	x8, x8, #4                          ; alu
	b.ne	LBB97_114                           ; branch
## hot LBB97_117 (blocks [128])
LBB97_117:
	ldrsh	w11, [x9], #2                      ; load
	cmp	w22, w11                             ; test
	cinc	x8, x8, gt                          ; alu
	adds	x10, x10, #1                        ; alu
	b.lo	LBB97_117                           ; branch
## window LBB97_5 (blocks [7, 8, 9, 10, 11])
LBB97_5:
	ldr	w8, [x27, #128]                      ; load
	ldr	x9, [x27, #80]                       ; load
	cmp	x9, #0                               ; test
	csel	x9, x22, x20, eq                    ; alu
	ldr	w9, [x27, x9]                        ; gather
	cmp	w8, w9                               ; test
	b.lo	LBB97_3                             ; branch
	ldr	x0, [x19, #8]                        ; load
	ldr	x8, [x0, #40]                        ; load
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB97_118                  ; branch
	cbz	x0, LBB97_119                        ; branch
	ldr	x8, [x19, #24]                       ; load
	cbz	x8, LBB97_5                          ; branch
	ldaprb	w8, [x8]                          ; load
	cbz	w8, LBB97_5                          ; branch
## window LBB97_21 (blocks [23, 24, 25, 26, 27])
LBB97_21:
	ldr	w8, [x27, #128]                      ; load
	ldr	x9, [x27, #80]                       ; load
	cmp	x9, #0                               ; test
	csel	x9, x25, x24, eq                    ; alu
	ldr	w9, [x27, x9]                        ; gather
	cmp	w8, w9                               ; test
	b.lo	LBB97_26                            ; branch
	ldr	x0, [x19, #8]                        ; load
	ldr	x8, [x0, #40]                        ; load
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB97_118                  ; branch
	cbz	x0, LBB97_119                        ; branch
	ldr	x8, [x19, #24]                       ; load
	cbz	x8, LBB97_21                         ; branch
	ldaprb	w8, [x8]                          ; load
	cbz	w8, LBB97_21                         ; branch
