backend/utils/mb/mbutils/src/lib.rs	CLIENT_ENCODING	pgport	at:SetClientEncoding	leader	client encoding conversion runs on the wire; client_encoding, written by SetClientEncoding (the GUC assign: SET / RESET)
backend/utils/mb/mbutils/src/lib.rs	DATABASE_ENCODING	claim	claim	claim	collations
backend/utils/mb/mbutils/src/lib.rs	MESSAGE_ENCODING	pgport	exit	thread	message translation (no NLS build); set once at startup by SetMessageEncoding
backend/utils/mb/mbutils/src/lib.rs	BACKEND_STARTUP_COMPLETE	pgport	exit	leader	client encoding conversion runs on the wire; set once by InitializeClientEncoding
backend/utils/mb/mbutils/src/lib.rs	PENDING_CLIENT_ENCODING	pgport	exit	leader	client encoding conversion runs on the wire; the pre-startup client_encoding, read once by InitializeClientEncoding
backend/utils/mb/mbutils/src/lib.rs	TO_SERVER_CONV_PROC	pgport	at:SetClientEncoding	leader	client encoding conversion runs on the wire; cleared or replaced by SetClientEncoding on a client_encoding change
backend/utils/mb/mbutils/src/lib.rs	TO_CLIENT_CONV_PROC	pgport	at:SetClientEncoding	leader	client encoding conversion runs on the wire; cleared or replaced by SetClientEncoding on a client_encoding change
backend/utils/mb/mbutils/src/lib.rs	UTF8_TO_SERVER_CONV_PROC	pgport	exit	leader	client encoding conversion runs on the wire; UTF8-to-server proc resolved once by InitializeClientEncoding
backend/utils/mb/mbutils/src/lib.rs	CONV_PROC_LIST	pgport	exit	leader	client encoding conversion runs on the wire; C's ConvProcList for the backend's life (SetClientEncoding prunes duplicates); at most one entry per client encoding, per session by design (mbutils.c's ConvProcList)
backend/utils/mb/mbutils/src/lib.rs	STATEMENT_CONVERSIONS	claim	claim	claim	catalog
