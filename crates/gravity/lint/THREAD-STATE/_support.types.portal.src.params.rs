_support/types/portal/src/params.rs	ENTRIES	pgport	at:free	leader	the parameter lists behind ParamListHandles; a handle resolves only on the thread that registered it (lookup panics elsewhere, slot reads None); bounded by the live lists: free empties each slot
_support/types/portal/src/params.rs	FREE	pgport	at:free	leader	the free slots of ENTRIES, reused by register; bounded by ENTRIES' length
_support/types/portal/src/params.rs	GENERATION	pgport	exit	leader	the generation counter stamped into each handle register makes
