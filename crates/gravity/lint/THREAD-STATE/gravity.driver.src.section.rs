gravity/driver/src/section.rs	KEPT_PROBE_BUF	runtime	call	scratch	kept_run's 1 MiB probe buffer, reused per owner phase (only its first page zeroed per use)
gravity/driver/src/section.rs	OWN_BUFS	runtime	call	scratch	own_all's partition and snapshot buffers, taken by OwnBufs::take and handed back by its Drop
