gravity/fn/src/srf.rs	SLOTS	runtime	statement:release_slots	thread	per-level SRF slot contexts on this thread; release_slots resets them at statement end / error; trim_levels drops deep levels
gravity/fn/src/srf.rs	LEVEL	runtime	scope:at_level	thread	the re-entry level whose SRF slot bank is in use; at_level's Back guard restores it
gravity/fn/src/srf.rs	HELD	runtime	statement:release_slots	thread	per-level slots live executions hold (reserve_slots); released at statement end / error by release_slots
