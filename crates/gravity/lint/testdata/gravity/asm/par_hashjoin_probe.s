# gravity-asm corpus: kernel par_hashjoin_probe
# entry: gravity_kernels::par_hashjoin::par_hashjoin_probe
# symbol: gravity_kernels::par_hashjoin::par_hashjoin_probe
# arch arm64 Darwin; rustc 1.96.0 (ac68faa20 2026-05-25); profile dist (lto fat, codegen-units 1); 2026-09-16T15:49Z; af7d08b382e
# function: 4158 instructions, 867 blocks, 85 loops, 56 innermost: 45 hot, 11 window, 0 other
# hot LBB96_21: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_49: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_76: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_102: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_127: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_153: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_179: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_204: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_229: 37 instructions in 10 blocks, loads 9 (gather 4), stores 1, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_309: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_318: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_333: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_342: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_357: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_366: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_384: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_392: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_407: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_416: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_431: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_440: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_456: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_465: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_481: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_490: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_505: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_514: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_549: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_558: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_573: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_582: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_597: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_606: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_621: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_630: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_645: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_654: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_669: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_678: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_693: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_702: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_717: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_726: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_741: 10 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB96_750: 11 instructions in 3 blocks, loads 1 (gather 1), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# window LBB96_9: 17 instructions in 5 blocks, loads 7 (gather 1), stores 0, tests 2, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
# window LBB96_36: 41 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
# window LBB96_63: 44 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_91: 48 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_116: 39 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
# window LBB96_142: 46 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_168: 41 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
# window LBB96_193: 42 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_218: 42 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_244: 48 instructions in 8 blocks, loads 9 (gather 0), stores 7, tests 5, flag-or 0, calls 1 (indirect 1, cold 0), stack 1
# window LBB96_260: 17 instructions in 5 blocks, loads 7 (gather 1), stores 0, tests 2, flag-or 0, calls 1 (indirect 1, cold 0), stack 0
## hot LBB96_21 (blocks [26, 27, 28, 29, 30, 31, 32, 34, 35, 36])
LBB96_21:
	add	x8, x27, #16                         ; alu
	cmp	w8, w0                               ; test
	b.hs	LBB96_26                            ; branch
	cbz	x1, LBB96_24                         ; branch
	ldr	w8, [x1, w8, uxtw #2]                ; gather
	b	LBB96_25                               ; branch
LBB96_24:
	mov	w8, w8                               ; alu
LBB96_25:
	ldr	w8, [x12, x8, lsl #2]                ; gather
	ldr	w9, [x26]                            ; load
	ldrb	w10, [x26, #12]                     ; load
	lsr	x9, x9, x10                          ; alu
	mul	w8, w8, w13                          ; alu
	rbit	w11, w8                             ; alu
	lsl	w10, w14, w10                        ; alu
	bic	w10, w11, w10                        ; alu
	umull	x10, w9, w10                       ; alu
	sub	w9, w9, #1                           ; alu
	and	x8, x9, x8                           ; alu
	add	x9, x23, x10, lsl #4                 ; alu
	add	x8, x9, x8, lsl #4                   ; alu
	prfm	pldl1keep, [x8]                     ; load
LBB96_26:
	cbz	x1, LBB96_28                         ; branch
	ldr	w8, [x1, x27, lsl #2]                ; gather
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbnz	x10, LBB96_29                       ; branch
LBB96_28:
	mov	w8, w27                              ; alu
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbz	x10, LBB96_31                        ; branch
LBB96_29:
	ldr	x10, [x10, x9, lsl #3]               ; gather
	lsr	x10, x10, x8                         ; alu
	tbnz	w10, #0, LBB96_31                   ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x0                              ; test
	b.lo	LBB96_21                            ; branch
## hot LBB96_49 (blocks [58, 59, 60, 61, 62, 63, 64, 66, 67, 68])
LBB96_49:
	add	x9, x27, #16                         ; alu
	cmp	w9, w1                               ; test
	b.hs	LBB96_54                            ; branch
	cbz	x8, LBB96_52                         ; branch
	ldr	w9, [x8, w9, uxtw #2]                ; gather
	b	LBB96_53                               ; branch
LBB96_52:
	mov	w9, w9                               ; alu
LBB96_53:
	ldrsh	w9, [x17, x9, lsl #1]              ; gather
	ldr	w10, [x26]                           ; load
	ldrb	w11, [x26, #12]                     ; load
	lsr	x10, x10, x11                        ; alu
	mul	w9, w9, w13                          ; alu
	rbit	w12, w9                             ; alu
	lsl	w11, w14, w11                        ; alu
	bic	w11, w12, w11                        ; alu
	umull	x11, w10, w11                      ; alu
	sub	w10, w10, #1                         ; alu
	and	x9, x10, x9                          ; alu
	add	x10, x23, x11, lsl #4                ; alu
	add	x9, x10, x9, lsl #4                  ; alu
	prfm	pldl1keep, [x9]                     ; load
LBB96_54:
	cbz	x8, LBB96_56                         ; branch
	ldr	w9, [x8, x27, lsl #2]                ; gather
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbnz	x11, LBB96_57                       ; branch
LBB96_56:
	mov	w9, w27                              ; alu
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbz	x11, LBB96_59                        ; branch
LBB96_57:
	ldr	x11, [x11, x10, lsl #3]              ; gather
	lsr	x11, x11, x9                         ; alu
	tbnz	w11, #0, LBB96_59                   ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x1                              ; test
	b.lo	LBB96_49                            ; branch
## hot LBB96_76 (blocks [89, 90, 91, 92, 93, 94, 95, 97, 98, 99])
LBB96_76:
	add	x8, x27, #16                         ; alu
	cmp	w8, w0                               ; test
	b.hs	LBB96_81                            ; branch
	cbz	x1, LBB96_79                         ; branch
	ldr	w8, [x1, w8, uxtw #2]                ; gather
	b	LBB96_80                               ; branch
LBB96_79:
	mov	w8, w8                               ; alu
LBB96_80:
	ldr	x8, [x12, x8, lsl #3]                ; gather
	ldr	w9, [x26]                            ; load
	ldrb	w10, [x26, #12]                     ; load
	lsr	x9, x9, x10                          ; alu
	mul	w8, w8, w13                          ; alu
	rbit	w11, w8                             ; alu
	lsl	w10, w14, w10                        ; alu
	bic	w10, w11, w10                        ; alu
	umull	x10, w9, w10                       ; alu
	sub	w9, w9, #1                           ; alu
	and	x8, x9, x8                           ; alu
	add	x9, x23, x10, lsl #4                 ; alu
	add	x8, x9, x8, lsl #4                   ; alu
	prfm	pldl1keep, [x8]                     ; load
LBB96_81:
	cbz	x1, LBB96_83                         ; branch
	ldr	w8, [x1, x27, lsl #2]                ; gather
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbnz	x10, LBB96_84                       ; branch
LBB96_83:
	mov	w8, w27                              ; alu
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbz	x10, LBB96_86                        ; branch
LBB96_84:
	ldr	x10, [x10, x9, lsl #3]               ; gather
	lsr	x10, x10, x8                         ; alu
	tbnz	w10, #0, LBB96_86                   ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x0                              ; test
	b.lo	LBB96_76                            ; branch
## hot LBB96_102 (blocks [119, 120, 121, 122, 123, 124, 125, 127, 128, 129])
LBB96_102:
	add	x9, x27, #16                         ; alu
	cmp	w9, w1                               ; test
	b.hs	LBB96_107                           ; branch
	cbz	x8, LBB96_105                        ; branch
	ldr	w9, [x8, w9, uxtw #2]                ; gather
	b	LBB96_106                              ; branch
LBB96_105:
	mov	w9, w9                               ; alu
LBB96_106:
	ldr	w9, [x17, x9, lsl #2]                ; gather
	ldr	w10, [x26]                           ; load
	ldrb	w11, [x26, #12]                     ; load
	lsr	x10, x10, x11                        ; alu
	mul	w9, w9, w13                          ; alu
	rbit	w12, w9                             ; alu
	lsl	w11, w14, w11                        ; alu
	bic	w11, w12, w11                        ; alu
	umull	x11, w10, w11                      ; alu
	sub	w10, w10, #1                         ; alu
	and	x9, x10, x9                          ; alu
	add	x10, x23, x11, lsl #4                ; alu
	add	x9, x10, x9, lsl #4                  ; alu
	prfm	pldl1keep, [x9]                     ; load
LBB96_107:
	cbz	x8, LBB96_109                        ; branch
	ldr	w9, [x8, x27, lsl #2]                ; gather
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbnz	x11, LBB96_110                      ; branch
LBB96_109:
	mov	w9, w27                              ; alu
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbz	x11, LBB96_112                       ; branch
LBB96_110:
	ldr	x11, [x11, x10, lsl #3]              ; gather
	lsr	x11, x11, x9                         ; alu
	tbnz	w11, #0, LBB96_112                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x1                              ; test
	b.lo	LBB96_102                           ; branch
## hot LBB96_127 (blocks [148, 149, 150, 151, 152, 153, 154, 156, 157, 158])
LBB96_127:
	add	x8, x27, #16                         ; alu
	cmp	w8, w0                               ; test
	b.hs	LBB96_132                           ; branch
	cbz	x1, LBB96_130                        ; branch
	ldr	w8, [x1, w8, uxtw #2]                ; gather
	b	LBB96_131                              ; branch
LBB96_130:
	mov	w8, w8                               ; alu
LBB96_131:
	ldrsh	w8, [x12, x8, lsl #1]              ; gather
	ldr	w9, [x26]                            ; load
	ldrb	w10, [x26, #12]                     ; load
	lsr	x9, x9, x10                          ; alu
	mul	w8, w8, w13                          ; alu
	rbit	w11, w8                             ; alu
	lsl	w10, w14, w10                        ; alu
	bic	w10, w11, w10                        ; alu
	umull	x10, w9, w10                       ; alu
	sub	w9, w9, #1                           ; alu
	and	x8, x9, x8                           ; alu
	add	x9, x23, x10, lsl #4                 ; alu
	add	x8, x9, x8, lsl #4                   ; alu
	prfm	pldl1keep, [x8]                     ; load
LBB96_132:
	cbz	x1, LBB96_134                        ; branch
	ldr	w8, [x1, x27, lsl #2]                ; gather
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbnz	x10, LBB96_135                      ; branch
LBB96_134:
	mov	w8, w27                              ; alu
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbz	x10, LBB96_137                       ; branch
LBB96_135:
	ldr	x10, [x10, x9, lsl #3]               ; gather
	lsr	x10, x10, x8                         ; alu
	tbnz	w10, #0, LBB96_137                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x0                              ; test
	b.lo	LBB96_127                           ; branch
## hot LBB96_153 (blocks [178, 179, 180, 181, 182, 183, 184, 186, 187, 188])
LBB96_153:
	add	x8, x27, #16                         ; alu
	cmp	w8, w0                               ; test
	b.hs	LBB96_158                           ; branch
	cbz	x1, LBB96_156                        ; branch
	ldr	w8, [x1, w8, uxtw #2]                ; gather
	b	LBB96_157                              ; branch
LBB96_156:
	mov	w8, w8                               ; alu
LBB96_157:
	ldr	w8, [x12, x8, lsl #2]                ; gather
	ldr	w9, [x26]                            ; load
	ldrb	w10, [x26, #12]                     ; load
	lsr	x9, x9, x10                          ; alu
	mul	w8, w8, w13                          ; alu
	rbit	w11, w8                             ; alu
	lsl	w10, w14, w10                        ; alu
	bic	w10, w11, w10                        ; alu
	umull	x10, w9, w10                       ; alu
	sub	w9, w9, #1                           ; alu
	and	x8, x9, x8                           ; alu
	add	x9, x23, x10, lsl #4                 ; alu
	add	x8, x9, x8, lsl #4                   ; alu
	prfm	pldl1keep, [x8]                     ; load
LBB96_158:
	cbz	x1, LBB96_160                        ; branch
	ldr	w8, [x1, x27, lsl #2]                ; gather
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbnz	x10, LBB96_161                      ; branch
LBB96_160:
	mov	w8, w27                              ; alu
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbz	x10, LBB96_163                       ; branch
LBB96_161:
	ldr	x10, [x10, x9, lsl #3]               ; gather
	lsr	x10, x10, x8                         ; alu
	tbnz	w10, #0, LBB96_163                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x0                              ; test
	b.lo	LBB96_153                           ; branch
## hot LBB96_179 (blocks [208, 209, 210, 211, 212, 213, 214, 216, 217, 218])
LBB96_179:
	add	x9, x27, #16                         ; alu
	cmp	w9, w1                               ; test
	b.hs	LBB96_184                           ; branch
	cbz	x8, LBB96_182                        ; branch
	ldr	w9, [x8, w9, uxtw #2]                ; gather
	b	LBB96_183                              ; branch
LBB96_182:
	mov	w9, w9                               ; alu
LBB96_183:
	ldrsh	w9, [x17, x9, lsl #1]              ; gather
	ldr	w10, [x26]                           ; load
	ldrb	w11, [x26, #12]                     ; load
	lsr	x10, x10, x11                        ; alu
	mul	w9, w9, w13                          ; alu
	rbit	w12, w9                             ; alu
	lsl	w11, w14, w11                        ; alu
	bic	w11, w12, w11                        ; alu
	umull	x11, w10, w11                      ; alu
	sub	w10, w10, #1                         ; alu
	and	x9, x10, x9                          ; alu
	add	x10, x23, x11, lsl #4                ; alu
	add	x9, x10, x9, lsl #4                  ; alu
	prfm	pldl1keep, [x9]                     ; load
LBB96_184:
	cbz	x8, LBB96_186                        ; branch
	ldr	w9, [x8, x27, lsl #2]                ; gather
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbnz	x11, LBB96_187                      ; branch
LBB96_186:
	mov	w9, w27                              ; alu
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbz	x11, LBB96_189                       ; branch
LBB96_187:
	ldr	x11, [x11, x10, lsl #3]              ; gather
	lsr	x11, x11, x9                         ; alu
	tbnz	w11, #0, LBB96_189                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x1                              ; test
	b.lo	LBB96_179                           ; branch
## hot LBB96_204 (blocks [237, 238, 239, 240, 241, 242, 243, 245, 246, 247])
LBB96_204:
	add	x9, x27, #16                         ; alu
	cmp	w9, w1                               ; test
	b.hs	LBB96_209                           ; branch
	cbz	x8, LBB96_207                        ; branch
	ldr	w9, [x8, w9, uxtw #2]                ; gather
	b	LBB96_208                              ; branch
LBB96_207:
	mov	w9, w9                               ; alu
LBB96_208:
	ldr	x9, [x17, x9, lsl #3]                ; gather
	ldr	w10, [x26]                           ; load
	ldrb	w11, [x26, #12]                     ; load
	lsr	x10, x10, x11                        ; alu
	mul	w9, w9, w13                          ; alu
	rbit	w12, w9                             ; alu
	lsl	w11, w14, w11                        ; alu
	bic	w11, w12, w11                        ; alu
	umull	x11, w10, w11                      ; alu
	sub	w10, w10, #1                         ; alu
	and	x9, x10, x9                          ; alu
	add	x10, x23, x11, lsl #4                ; alu
	add	x9, x10, x9, lsl #4                  ; alu
	prfm	pldl1keep, [x9]                     ; load
LBB96_209:
	cbz	x8, LBB96_211                        ; branch
	ldr	w9, [x8, x27, lsl #2]                ; gather
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbnz	x11, LBB96_212                      ; branch
LBB96_211:
	mov	w9, w27                              ; alu
	ldr	x11, [x16, #8]                       ; load
	lsr	x10, x9, #6                          ; alu
	cbz	x11, LBB96_214                       ; branch
LBB96_212:
	ldr	x11, [x11, x10, lsl #3]              ; gather
	lsr	x11, x11, x9                         ; alu
	tbnz	w11, #0, LBB96_214                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x1                              ; test
	b.lo	LBB96_204                           ; branch
## hot LBB96_229 (blocks [266, 267, 268, 269, 270, 271, 272, 274, 275, 276])
LBB96_229:
	add	x8, x27, #16                         ; alu
	cmp	w8, w0                               ; test
	b.hs	LBB96_234                           ; branch
	cbz	x1, LBB96_232                        ; branch
	ldr	w8, [x1, w8, uxtw #2]                ; gather
	b	LBB96_233                              ; branch
LBB96_232:
	mov	w8, w8                               ; alu
LBB96_233:
	ldr	x8, [x12, x8, lsl #3]                ; gather
	ldr	w9, [x26]                            ; load
	ldrb	w10, [x26, #12]                     ; load
	lsr	x9, x9, x10                          ; alu
	mul	w8, w8, w13                          ; alu
	rbit	w11, w8                             ; alu
	lsl	w10, w14, w10                        ; alu
	bic	w10, w11, w10                        ; alu
	umull	x10, w9, w10                       ; alu
	sub	w9, w9, #1                           ; alu
	and	x8, x9, x8                           ; alu
	add	x9, x23, x10, lsl #4                 ; alu
	add	x8, x9, x8, lsl #4                   ; alu
	prfm	pldl1keep, [x8]                     ; load
LBB96_234:
	cbz	x1, LBB96_236                        ; branch
	ldr	w8, [x1, x27, lsl #2]                ; gather
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbnz	x10, LBB96_237                      ; branch
LBB96_236:
	mov	w8, w27                              ; alu
	ldr	x10, [x16, #8]                       ; load
	lsr	x9, x8, #6                           ; alu
	cbz	x10, LBB96_239                       ; branch
LBB96_237:
	ldr	x10, [x10, x9, lsl #3]               ; gather
	lsr	x10, x10, x8                         ; alu
	tbnz	w10, #0, LBB96_239                  ; branch
	str	xzr, [x20, #304]                     ; store
	add	x27, x27, #1                         ; alu
	cmp	x27, x0                              ; test
	b.lo	LBB96_229                           ; branch
## hot LBB96_309 (blocks [351, 352, 353])
LBB96_309:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_300                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_315                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_309                           ; branch
## hot LBB96_318 (blocks [361, 362, 363])
LBB96_318:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_323                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_318                           ; branch
## hot LBB96_333 (blocks [377, 378, 379])
LBB96_333:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_325                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_339                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_333                           ; branch
## hot LBB96_342 (blocks [387, 388, 389])
LBB96_342:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_347                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_342                           ; branch
## hot LBB96_357 (blocks [403, 404, 405])
LBB96_357:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_349                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_363                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_357                           ; branch
## hot LBB96_366 (blocks [413, 414, 415])
LBB96_366:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_371                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_366                           ; branch
## hot LBB96_384 (blocks [432, 433, 434])
LBB96_384:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_373                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_389                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_384                           ; branch
## hot LBB96_392 (blocks [441, 442, 443])
LBB96_392:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_397                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_392                           ; branch
## hot LBB96_407 (blocks [457, 458, 459])
LBB96_407:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_399                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_413                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_407                           ; branch
## hot LBB96_416 (blocks [467, 468, 469])
LBB96_416:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_421                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_416                           ; branch
## hot LBB96_431 (blocks [483, 484, 485])
LBB96_431:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_423                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_437                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_431                           ; branch
## hot LBB96_440 (blocks [493, 494, 495])
LBB96_440:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_445                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_440                           ; branch
## hot LBB96_456 (blocks [510, 511, 512])
LBB96_456:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_447                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_462                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_456                           ; branch
## hot LBB96_465 (blocks [520, 521, 522])
LBB96_465:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_470                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_465                           ; branch
## hot LBB96_481 (blocks [537, 538, 539])
LBB96_481:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_472                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_487                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_481                           ; branch
## hot LBB96_490 (blocks [547, 548, 549])
LBB96_490:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_495                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_772                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_490                           ; branch
## hot LBB96_505 (blocks [563, 564, 565])
LBB96_505:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_497                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_511                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_505                           ; branch
## hot LBB96_514 (blocks [573, 574, 575])
LBB96_514:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_519                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_514                           ; branch
## hot LBB96_549 (blocks [609, 610, 611])
LBB96_549:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_541                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_555                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_549                           ; branch
## hot LBB96_558 (blocks [619, 620, 621])
LBB96_558:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_563                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_558                           ; branch
## hot LBB96_573 (blocks [635, 636, 637])
LBB96_573:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_565                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_579                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_573                           ; branch
## hot LBB96_582 (blocks [645, 646, 647])
LBB96_582:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_587                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_582                           ; branch
## hot LBB96_597 (blocks [661, 662, 663])
LBB96_597:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_589                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_603                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_597                           ; branch
## hot LBB96_606 (blocks [671, 672, 673])
LBB96_606:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_611                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_606                           ; branch
## hot LBB96_621 (blocks [687, 688, 689])
LBB96_621:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_613                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_627                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_621                           ; branch
## hot LBB96_630 (blocks [697, 698, 699])
LBB96_630:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_635                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_630                           ; branch
## hot LBB96_645 (blocks [713, 714, 715])
LBB96_645:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_637                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_651                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_645                           ; branch
## hot LBB96_654 (blocks [723, 724, 725])
LBB96_654:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_659                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_654                           ; branch
## hot LBB96_669 (blocks [739, 740, 741])
LBB96_669:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_661                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_675                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_669                           ; branch
## hot LBB96_678 (blocks [749, 750, 751])
LBB96_678:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_683                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_678                           ; branch
## hot LBB96_693 (blocks [765, 766, 767])
LBB96_693:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_685                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_699                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_693                           ; branch
## hot LBB96_702 (blocks [775, 776, 777])
LBB96_702:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_707                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_702                           ; branch
## hot LBB96_717 (blocks [791, 792, 793])
LBB96_717:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_709                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_723                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_717                           ; branch
## hot LBB96_726 (blocks [801, 802, 803])
LBB96_726:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_731                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_726                           ; branch
## hot LBB96_741 (blocks [817, 818, 819])
LBB96_741:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_733                       ; branch
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_747                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_741                           ; branch
## hot LBB96_750 (blocks [827, 828, 829])
LBB96_750:
	lsl	x12, x10, #4                         ; alu
	ldr	w12, [x11, x12]                      ; gather
	cbz	w12, LBB96_755                       ; branch
	mov	x0, #-518                            ; alu
	cmp	x8, x9                               ; test
	cinc	x8, x8, lo                          ; alu
	b.hs	LBB96_773                           ; branch
	add	x10, x10, #1                         ; alu
	and	x10, x10, x9                         ; alu
	cmp	x8, x9                               ; test
	b.ls	LBB96_750                           ; branch
## window LBB96_9 (blocks [13, 14, 15, 16, 17])
LBB96_9:
	ldr	w8, [x21, #320]                      ; load
	ldr	x9, [x21, #272]                      ; load
	cmp	x9, #0                               ; test
	csel	x9, x25, x24, eq                    ; alu
	ldr	w9, [x22, x9]                        ; gather
	cmp	w8, w9                               ; test
	b.lo	LBB96_14                            ; branch
	ldr	x8, [x19, #8]                        ; load
	ldr	x9, [x8, #232]                       ; load
	add	x0, x8, #192                         ; alu
	blr	x9                                   ; CALL indirect
	tbnz	x0, #63, LBB96_779                  ; branch
	cbz	x0, LBB96_762                        ; branch
	ldr	x8, [x19, #24]                       ; load
	cbz	x8, LBB96_9                          ; branch
	ldaprb	w8, [x8]                          ; load
	cbz	w8, LBB96_9                          ; branch
## window LBB96_36 (blocks [42, 43, 44, 45, 46, 47, 48, 49])
LBB96_35:
	sub	x23, x23, #1                         ; alu
LBB96_36:
	cbz	x23, LBB96_43                        ; branch
	add	x8, x24, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_43                         ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x25                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w28                             ; test
	b.ne	LBB96_35                            ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x28, [x11]                           ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x21, [x11, #16]                      ; store
	strb	w17, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_35                            ; branch
	mov	x0, x19                              ; alu
	mov	x26, x17                             ; alu
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_777                           ; branch
	tbnz	x0, #63, LBB96_769                  ; branch
	cmp	x0, #1                               ; test
	mov	x17, x26                             ; alu
	b.ne	LBB96_35                            ; branch
## window LBB96_63 (blocks [73, 74, 75, 76, 77, 78, 79, 80])
LBB96_62:
	sub	x24, x24, #1                         ; alu
LBB96_63:
	cbz	x24, LBB96_70                        ; branch
	add	x8, x28, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_70                         ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x21                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w25                             ; test
	b.ne	LBB96_62                            ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x2, [x11]                            ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x16, [x11, #16]                      ; store
	strb	w23, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_62                            ; branch
	mov	x0, x19                              ; alu
	mov	x20, x2                              ; alu
	mov	x26, x16                             ; alu
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_768                           ; branch
	tbnz	x0, #63, LBB96_765                  ; branch
	cmp	x0, #1                               ; test
	mov	x16, x26                             ; alu
	mov	x2, x20                              ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_62                            ; branch
## window LBB96_91 (blocks [105, 106, 107, 108, 109, 110, 111, 112])
LBB96_90:
	sub	x23, x23, #1                         ; alu
LBB96_91:
	cbz	x23, LBB96_98                        ; branch
	add	x8, x15, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_98                         ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x25                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w21                             ; test
	b.ne	LBB96_90                            ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x0, [x11]                            ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x24, [x11, #16]                      ; store
	strb	w17, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_90                            ; branch
	mov	x20, x0                              ; alu
	mov	x0, x19                              ; alu
	mov	x19, x15                             ; alu
	mov	x26, x17                             ; alu
	ldr	x8, [x0]                             ; load
	ldr	x8, [x8]                             ; load
	mov	x28, x0                              ; alu
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_770                           ; branch
	tbnz	x0, #63, LBB96_765                  ; branch
	cmp	x0, #1                               ; test
	mov	x17, x26                             ; alu
	mov	x15, x19                             ; alu
	mov	x19, x28                             ; alu
	mov	x0, x20                              ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_90                            ; branch
## window LBB96_116 (blocks [134, 135, 136, 137, 138, 139, 140, 141])
LBB96_115:
	sub	x28, x28, #1                         ; alu
LBB96_116:
	cbz	x28, LBB96_123                       ; branch
	add	x8, x25, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_123                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x26                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w24                             ; test
	b.ne	LBB96_115                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x24, [x11]                           ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x23, [x11, #16]                      ; store
	strb	w21, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_115                           ; branch
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	mov	x0, x19                              ; alu
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_764                           ; branch
	tbnz	x0, #63, LBB96_769                  ; branch
	cmp	x0, #1                               ; test
	b.ne	LBB96_115                           ; branch
## window LBB96_142 (blocks [164, 165, 166, 167, 168, 169, 170, 171])
LBB96_141:
	sub	x23, x23, #1                         ; alu
LBB96_142:
	cbz	x23, LBB96_149                       ; branch
	add	x8, x15, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_149                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x25                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w21                             ; test
	b.ne	LBB96_141                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x28, [x11]                           ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x24, [x11, #16]                      ; store
	strb	w17, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_141                           ; branch
	mov	x0, x19                              ; alu
	mov	x19, x15                             ; alu
	mov	x26, x17                             ; alu
	ldr	x8, [x0]                             ; load
	ldr	x8, [x8]                             ; load
	mov	x20, x0                              ; alu
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_775                           ; branch
	tbnz	x0, #63, LBB96_782                  ; branch
	cmp	x0, #1                               ; test
	mov	x17, x26                             ; alu
	mov	x15, x19                             ; alu
	mov	x19, x20                             ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_141                           ; branch
## window LBB96_168 (blocks [194, 195, 196, 197, 198, 199, 200, 201])
LBB96_167:
	sub	x23, x23, #1                         ; alu
LBB96_168:
	cbz	x23, LBB96_175                       ; branch
	add	x8, x24, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_175                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x25                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w28                             ; test
	b.ne	LBB96_167                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x28, [x11]                           ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x21, [x11, #16]                      ; store
	strb	w17, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_167                           ; branch
	mov	x0, x19                              ; alu
	mov	x26, x17                             ; alu
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_764                           ; branch
	tbnz	x0, #63, LBB96_769                  ; branch
	cmp	x0, #1                               ; test
	mov	x17, x26                             ; alu
	b.ne	LBB96_167                           ; branch
## window LBB96_193 (blocks [223, 224, 225, 226, 227, 228, 229, 230])
LBB96_192:
	sub	x23, x23, #1                         ; alu
LBB96_193:
	cbz	x23, LBB96_200                       ; branch
	add	x8, x28, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_200                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x26                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w21                             ; test
	b.ne	LBB96_192                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x2, [x11]                            ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x25, [x11, #16]                      ; store
	strb	w24, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_192                           ; branch
	mov	x0, x19                              ; alu
	mov	x20, x2                              ; alu
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_768                           ; branch
	tbnz	x0, #63, LBB96_765                  ; branch
	cmp	x0, #1                               ; test
	mov	x2, x20                              ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_192                           ; branch
## window LBB96_218 (blocks [252, 253, 254, 255, 256, 257, 258, 259])
LBB96_217:
	sub	x23, x23, #1                         ; alu
LBB96_218:
	cbz	x23, LBB96_225                       ; branch
	add	x8, x28, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_225                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x26                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w21                             ; test
	b.ne	LBB96_217                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x2, [x11]                            ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x25, [x11, #16]                      ; store
	strb	w24, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_217                           ; branch
	mov	x0, x19                              ; alu
	mov	x20, x2                              ; alu
	ldr	x8, [x19]                            ; load
	ldr	x8, [x8]                             ; load
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_768                           ; branch
	tbnz	x0, #63, LBB96_765                  ; branch
	cmp	x0, #1                               ; test
	mov	x2, x20                              ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_217                           ; branch
## window LBB96_244 (blocks [282, 283, 284, 285, 286, 287, 288, 289])
LBB96_243:
	sub	x23, x23, #1                         ; alu
LBB96_244:
	cbz	x23, LBB96_251                       ; branch
	add	x8, x15, x22, lsl #4                 ; alu
	ldr	w9, [x8]                             ; load
	cbz	w9, LBB96_251                        ; branch
	add	x10, x22, #1                         ; alu
	and	x22, x10, x25                        ; alu
	ldr	w10, [x8, #4]                        ; load
	cmp	w10, w21                             ; test
	b.ne	LBB96_243                           ; branch
	ldr	x10, [x19, #16]                      ; load
	ldp	x12, x11, [x10, #8]                  ; load
	add	x11, x11, x11, lsl #1                ; alu
	ldr	x13, [x10]                           ; load
	add	x12, x12, x11                        ; alu
	add	x11, x13, x11, lsl #3                ; alu
	cmp	w9, #2                               ; test
	cset	w9, eq                              ; alu
	ldr	x8, [x8, #8]                         ; load
	str	x0, [x11]                            ; store
	strb	wzr, [x12]                          ; store
	str	x8, [x11, #8]                        ; store
	strb	w9, [x12, #1]                       ; store
	str	x24, [x11, #16]                      ; store
	strb	w17, [x12, #2]                      ; store
	ldp	x8, x9, [x10, #16]                   ; load
	add	x8, x8, #1                           ; alu
	str	x8, [x10, #16]                       ; store
	cmp	x8, x9                               ; test
	b.lt	LBB96_243                           ; branch
	mov	x20, x0                              ; alu
	mov	x0, x19                              ; alu
	mov	x19, x15                             ; alu
	mov	x26, x17                             ; alu
	ldr	x8, [x0]                             ; load
	ldr	x8, [x8]                             ; load
	mov	x28, x0                              ; alu
	blr	x8                                   ; CALL indirect
	cmn	x0, #1                               ; test
	b.eq	LBB96_770                           ; branch
	tbnz	x0, #63, LBB96_765                  ; branch
	cmp	x0, #1                               ; test
	mov	x17, x26                             ; alu
	mov	x15, x19                             ; alu
	mov	x19, x28                             ; alu
	mov	x0, x20                              ; alu
	ldr	x20, [sp, #64]                       ; STACK load
	b.ne	LBB96_243                           ; branch
## window LBB96_260 (blocks [301, 302, 303, 304, 305])
LBB96_260:
	ldr	w8, [x20, #320]                      ; load
	ldr	x9, [x20, #272]                      ; load
	cmp	x9, #0                               ; test
	csel	x9, x25, x24, eq                    ; alu
	ldr	w9, [x23, x9]                        ; gather
	cmp	w8, w9                               ; test
	b.lo	LBB96_265                           ; branch
	ldr	x8, [x19, #8]                        ; load
	ldr	x9, [x8, #232]                       ; load
	add	x0, x8, #192                         ; alu
	blr	x9                                   ; CALL indirect
	tbnz	x0, #63, LBB96_779                  ; branch
	cbz	x0, LBB96_758                        ; branch
	ldr	x8, [x19, #24]                       ; load
	cbz	x8, LBB96_260                        ; branch
	ldaprb	w8, [x8]                          ; load
	cbz	w8, LBB96_260                        ; branch
