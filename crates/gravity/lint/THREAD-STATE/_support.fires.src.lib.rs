_support/fires/src/lib.rs	ON	pgport	exit	thread	this thread's reading of the report knob (PGRUST_GRAVITY_REPORT, or set), one u8
_support/fires/src/lib.rs	NAMES	pgport	at:drain	thread	report knob on only: the fire sites this thread hit since the last drain, at most one entry per site name; drain takes them
_support/fires/src/lib.rs	SEEN	pgport	key:drain number	cache	report knob on only: the names this thread put in SHARED since drain number .0, at most one entry per site name; a new drain number empties it
