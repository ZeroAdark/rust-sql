gravity/driver/src/mem/grant.rs	SLOTS_HERE	memory	scope:Reservation	thread	this thread's reservations: in-flight slots held; given back at park_slot and the Reservation's Drop
gravity/driver/src/mem/grant.rs	HELD_HERE	memory	scope:StmtCtx	thread	this thread's reservations: pool bytes its live statements hold; given back by the root StmtCtx's Drop
gravity/driver/src/mem/grant.rs	HOLDERS_HERE	memory	scope:StmtCtx	thread	this thread's reservations: live grant holders, counted from StmtCtx::root to its Drop
gravity/driver/src/mem/grant.rs	IDLE_HERE	memory	scope:Reservation	thread	this thread's reservations: holders whose portal is suspended; note_idle(false) at unpark or the Reservation's Drop
gravity/driver/src/mem/grant.rs	MEM_WAITING	memory	scope:MemWait	thread	this thread's outermost memory wait reports its wait event; cleared by the MemWait guard's Drop
gravity/driver/src/mem/grant.rs	CLAIM_PURSE	memory	scope:ClaimPurse	thread	D4043: the running claim's purse, set per claim by claim_purse; the ClaimPurse guard's Drop puts the previous back
gravity/driver/src/mem/grant.rs	STARTS	memory	test	test	test-only counter (cfg(test) module)
gravity/driver/src/mem/grant.rs	ENDS	memory	test	test	test-only counter (cfg(test) module)
