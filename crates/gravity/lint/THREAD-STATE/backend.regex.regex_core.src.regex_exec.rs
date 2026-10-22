backend/regex/regex_core/src/regex_exec.rs	EXEC_SCRATCH	pgport	call	scratch	the exec's reusable work arrays: each exec takes the box and puts it back
backend/regex/regex_core/src/regex_exec.rs	SUBDFA_POOL	pgport	call	scratch	freed small sub-DFA spaces, popped by SubDfa::new and pushed back on drop (at most SUBDFA_POOL_CAP, C's LOCALDFAS)
