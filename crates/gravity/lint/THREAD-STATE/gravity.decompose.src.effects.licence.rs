gravity/decompose/src/effects/licence.rs	DENY	licence	scope:deny_as	leader	lowering scope: every licence denied (plan-order run); put back by deny_as' guard
gravity/decompose/src/effects/licence.rs	STMT	licence	scope:lowering	leader	lowering scope: the statement acts / holds a recheck licence; put back by lowering's guard
gravity/decompose/src/effects/licence.rs	GRANTS	licence	exit	leader	the leader thread's licence grants: monotonic wrapping count bumped by licence(); exec admission reads it as a delta around one lowering (grants), never reset
gravity/decompose/src/effects/licence.rs	ACTED	licence	at:take_acted	leader	the leader thread's flag: a lowering of a statement that acts was entered; set by lowering(), taken (reset) by take_acted around one admission (exec or_plan_order)
