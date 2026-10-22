backend/executor/spi/src/lib.rs	SPI_STACK	pgport	xact:AtEOXact_SPI	leader	C's _SPI_stack: this backend's SPI connections (PL code, RI triggers, tier C builtins: none is pool-hosted); AtEOXact_SPI / AtEOSubXact_SPI unwind it
backend/executor/spi/src/lib.rs	SPI_CONNECTED	pgport	xact:AtEOXact_SPI	leader	C's _SPI_connected: the top connection's index in SPI_STACK
backend/executor/spi/src/lib.rs	SPI_PROCESSED	pgport	call	scratch	C's SPI_processed: the row count of this backend's last SPI execution, read by its caller right after
backend/executor/spi/src/lib.rs	SPI_RESULT	pgport	call	scratch	C's SPI_result: the error code of this backend's last SPI call, read by its caller right after
backend/executor/spi/src/lib.rs	SPI_TUPTABLE	pgport	call	scratch	C's SPI_tuptable: the result table of this backend's last SPI execution, read by its caller right after
