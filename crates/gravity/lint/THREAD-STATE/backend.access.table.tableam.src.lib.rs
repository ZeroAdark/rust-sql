backend/access/table/tableam/src/lib.rs	DEFAULT_TABLE_ACCESS_METHOD_GUC	pgport	claim	claim	gucs
backend/access/table/tableam/src/lib.rs	SCRATCH	pgrc	call	scratch	pgrc2 F1 L7a: the pgrc statement-row bump context, taken by each buffered_insert / handed bulk tuple_insert, reset and put back before the call returns; holds no row past the call
backend/access/table/tableam/src/lib.rs	CB2_BUFFER_HOOKS	pgrc	exit	thread	once-per-thread latch: this thread's pgrc statement-buffer xact / subxact callback registration (ensure_pgrc_buffer_hooks); a pool thread that begins a pgrc scan registers its own
