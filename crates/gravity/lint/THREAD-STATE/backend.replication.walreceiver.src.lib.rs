backend/replication/walreceiver/src/lib.rs	STATE	pgport	exit	thread	the walreceiver's own receive-file and write / flush / reply positions (C's file statics), walreceiver thread only; WalRcvDie closes its file
backend/replication/walreceiver/src/lib.rs	CONN	pgport	at:WalRcvDie	thread	wrconn: the walreceiver's connection to the primary, opened in WalReceiverMain, taken and closed by its on_shmem_exit WalRcvDie
backend/replication/walreceiver/src/lib.rs	STARTPOINT_TLI	pgport	exit	thread	the walreceiver's streaming timeline (C's on_shmem_exit argument), set per streaming start, read by WalRcvDie
