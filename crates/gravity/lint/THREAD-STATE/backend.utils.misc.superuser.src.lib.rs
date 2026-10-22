backend/utils/misc/superuser/src/lib.rs	LAST_ROLEID	pgport	key:roleid	cache	C's last_roleid: the role superuser_arg last answered for; RoleidCallback clears it on a pg_authid invalidation
backend/utils/misc/superuser/src/lib.rs	LAST_ROLEID_IS_SUPER	pgport	key:roleid	cache	C's last_roleid_is_super: the answer for LAST_ROLEID, read only when the role matches
backend/utils/misc/superuser/src/lib.rs	ROLEID_CALLBACK_REGISTERED	pgport	exit	thread	C's roleid_callback_registered: this thread registered RoleidCallback with its syscache once
