gravity/exec/src/phases.rs	MARKS	admission	statement:reset	leader	statement phase marks: fixed mark array; reset (simple_at / portal bind start) sets N to 0 so earlier marks go unread
gravity/exec/src/phases.rs	N	admission	statement:reset	leader	statement phase marks: marks taken this statement; reset at each runner statement start (execute simple_at, portal bind)
gravity/exec/src/phases.rs	T0	admission	statement:reset	leader	statement phase marks: the statement's start instant (None when no switch reads marks); reset at each runner statement start
