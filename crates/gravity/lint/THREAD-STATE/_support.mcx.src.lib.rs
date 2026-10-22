_support/mcx/src/lib.rs	ACCT	memory	at:passivate_trim	thread	memory crate, census-only: this thread's context accounting; pooled AcctInner shells, drained by the idle trim
_support/mcx/src/lib.rs	CHILD_VECS	memory	at:passivate_trim	thread	memory crate, census-only: this thread's context accounting; pooled child vectors, drained by the idle trim; capped at CHILD_VEC_POOL_MAX (64) vectors (its LocalStack)
_support/mcx/src/lib.rs	RETIRED_ROOTS	memory	exit	thread	memory crate, census-only: this thread's context accounting; retired session-root shells, freed at thread exit
_support/mcx/src/lib.rs	THREAD	memory	exit	thread	memory crate, census-only: this thread's context accounting; running bytes / peak (peak rebased on read)
_support/mcx/src/lib.rs	SESSION_ROOT_RETIRING	memory	scope:retire_session_root	thread	memory crate, census-only: this thread's context accounting; set while a session root retires, put back by its guard
