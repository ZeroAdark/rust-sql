gravity/decompose/src/admit.rs	PARTIAL	admission	scope:reading_partial	leader	admission scope: the root reader is partial (a cursor FETCH); put back by reading_partial's guard
gravity/decompose/src/admit.rs	RELSETS	admission	scope:grouping_relsets	leader	admission scope: Append twin heap scans read as one relation set; put back by grouping_relsets' guard
gravity/decompose/src/admit.rs	UNION_NESTS	admission	scope:copying_union_nests	leader	admission scope: D4569 a SELECT's quiet nest copied into its union arms; put back by copying_union_nests' guard
gravity/decompose/src/admit.rs	ROWS	admission	scope:rescaling	leader	admission scope: a generic plan's re-estimated node rows; put back by rescaling's guard
