backend/utils/misc/stack_depth_core/src/lib.rs	MAX_STACK_DEPTH	pgport	claim	claim	gucs
backend/utils/misc/stack_depth_core/src/lib.rs	MAX_STACK_DEPTH_BYTES	pgport	claim	claim	gucs
backend/utils/misc/stack_depth_core/src/lib.rs	SCALED_STACK_BUDGET	pgport	claim	claim	gucs
backend/utils/misc/stack_depth_core/src/lib.rs	THREAD_STACK_CEILING	pgport	at:set_thread_stack_ceiling	thread	this thread's real stack reservation less its reserve, set at the thread's start; the max_stack_depth budget is clamped to it
backend/utils/misc/stack_depth_core/src/lib.rs	STACK_BASE_PTR	pgport	at:set_stack_base	thread	C's stack_base_ptr: this thread's stack base, set at its start; restore_stack_base puts a saved one back
backend/utils/misc/stack_depth_core/src/lib.rs	STACK_DEPTH_RLIMIT_CACHE	pgport	exit	thread	C get_stack_depth_rlimit's static: the process stack rlimit as this thread first read it (wasm32: the declared engine stack, set_wasm_stack_ceiling); the rlimit does not change at run time
backend/utils/misc/stack_depth_core/src/lib.rs	EXCEEDED	pgport	exit	thread	how many 54001 errors this thread has built (D3360): monotonic, read as deltas around a call (exceeded_count)
backend/utils/misc/stack_depth_core/src/lib.rs	ENGINE_CHARGE	pgport	scope:EngineLevel	thread	wasm32: the engine-stack charge of the nested statement levels running on this thread; each EngineLevel adds its bytes and its Drop takes them off, an unwind too
backend/utils/misc/stack_depth_core/src/lib.rs	WASM_SHADOW_CEILING	pgport	at:set_wasm_stack_ceiling	thread	wasm32: the shadow-stack half of this thread's ceiling, set once at its start
backend/utils/misc/stack_depth_core/src/lib.rs	WASM_ENGINE_CEILING	pgport	at:set_wasm_stack_ceiling	thread	wasm32: the engine-stack half of this thread's ceiling, set once at its start
