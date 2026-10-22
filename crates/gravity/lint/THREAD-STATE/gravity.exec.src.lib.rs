gravity/exec/src/lib.rs	SESSION_END	lifetime	at:teardown	leader	the backend's session: once-per-session flag that registered invalidate::session_end; teardown (that session end) clears it
gravity/exec/src/lib.rs	DEEP_KNOBS	lifetime	exit	leader	the backend's session: deep levels' report knob and memo slots, read once per thread from the process env (seam report_on / memo_slots)
gravity/exec/src/lib.rs	LAST_MISSES	admission	exit	leader	the backend's session: process-wide pool catalog miss count at this thread's last served report line (B2), a snapshot never reset
gravity/exec/src/lib.rs	ARENA_EXIT	lifetime	exit	leader	the backend's session: thread-end guard (D656) armed by graph_arena; its Drop releases the levels' admission arenas at the thread's end
