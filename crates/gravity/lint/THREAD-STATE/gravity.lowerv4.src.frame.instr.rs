gravity/lowerv4/src/frame/instr.rs	ON	lowering	scope:scope	leader	lowering scope: the lowering is instrumented; scope's Reset guard clears it on every exit
gravity/lowerv4/src/frame/instr.rs	CELLS	lowering	scope:scope	leader	lowering scope: instrumentation cells allocated; cleared at scope's start and taken at its end
gravity/lowerv4/src/frame/instr.rs	PLAN	lowering	scope:scope	leader	lowering scope: the instrumentation plan; reset at scope's start and by its Reset guard
gravity/lowerv4/src/frame/instr.rs	CURRENT	lowering	scope:scope	leader	lowering scope: the current pipeline's bounds (enter's Restore nests); cleared by scope's Reset guard
gravity/lowerv4/src/frame/instr.rs	BASE	lowering	scope:scope	leader	lowering scope: the open counter base (open_base / close_base); cleared by scope's Reset guard
gravity/lowerv4/src/frame/instr.rs	AHEAD	lowering	scope:scope	leader	lowering scope: the window run-ahead counter value; cleared by scope's Reset guard
gravity/lowerv4/src/frame/instr.rs	HOT	lowering	scope:scope	leader	lowering scope: the counted nodes of the open base; cleared by close_base and scope's Reset guard
gravity/lowerv4/src/frame/instr.rs	BASES	lowering	scope:scope	leader	lowering scope: closed counter bases, taken by carry; cleared by scope's Reset guard
