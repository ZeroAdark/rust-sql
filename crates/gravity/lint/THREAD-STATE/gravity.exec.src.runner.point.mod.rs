gravity/exec/src/runner/point/mod.rs	BUILT	lifetime	statement:plan_skip@gravity/exec/src/runner/point/nofrom.rs	leader	point path: hook A built the plan hook B sees next; plan_skip clears it per statement, serve takes it
gravity/exec/src/runner/point/mod.rs	RUNNING	lifetime	scope:Running	leader	point path: a run is on this thread (no re-entry, memlead 00:10:16Z); the Running guard puts it back on every exit
gravity/exec/src/runner/point/mod.rs	REPORT	lifetime	exit	leader	point path: PGRUST_GRAVITY_REPORT read once per thread; never reset
gravity/exec/src/runner/point/mod.rs	PLANCHECK	lifetime	exit	leader	point path: PGRUST_GRAVITY_POINT_PLANCHECK read once per thread; never reset
gravity/exec/src/runner/point/mod.rs	STATE	lifetime	at:drop_state_at_teardown	leader	point path: the run context GravityPointRun (a session root, reset after each run) and its wire node; at most one per session, dropped at session teardown
