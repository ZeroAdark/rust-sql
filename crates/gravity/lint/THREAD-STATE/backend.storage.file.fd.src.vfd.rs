backend/storage/file/fd/src/vfd.rs	FD	claim	claim	claim	temp_account
backend/storage/file/fd/src/vfd.rs	MAX_FILES_PER_PROCESS	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (max_files_per_process)
backend/storage/file/fd/src/vfd.rs	MAX_SAFE_FDS	pgport	exit	thread	the descriptor budget set_max_safe_fds computes at start (launch_backend hands it to each backend thread), never carried; scalar_global! storage per thread
backend/storage/file/fd/src/vfd.rs	DATA_SYNC_RETRY	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (data_sync_retry)
backend/storage/file/fd/src/vfd.rs	RECOVERY_INIT_SYNC_METHOD	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (recovery_init_sync_method)
backend/storage/file/fd/src/vfd.rs	FILE_EXTEND_METHOD	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (file_extend_method)
backend/storage/file/fd/src/vfd.rs	FILE_COPY_METHOD	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (file_copy_method)
backend/storage/file/fd/src/vfd.rs	IO_DIRECT_FLAGS	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; scalar_global! storage per thread (debug_io_direct's flags)
backend/storage/file/fd/src/vfd.rs	NUM_EXTERNAL_FDS	pgport	exit	thread	this thread's count of descriptors taken outside the VFD cache (AcquireExternalFD / ReleaseExternalFD); scalar_global! storage
backend/storage/file/fd/src/vfd.rs	TEMP_TABLESPACES_GUC	claim	claim	claim	gucs
backend/storage/file/fd/src/vfd.rs	SEEN_SHED_GEN	pgport	exit	thread	this thread's descriptor shedding: the last SHED_GEN it shed for (a monotonic stamp)
