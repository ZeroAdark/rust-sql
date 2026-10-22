backend/utils/adt/pg_locale/src/lib.rs	DEFAULT_LOCALE	claim	claim	claim	collations
backend/utils/adt/pg_locale/src/lib.rs	DEFAULT_LOCALE_DB	pgport	at:init_database_collation	leader	built only with a catcache; the database DEFAULT_LOCALE was built for, rewritten by init_database_collation / arm_default_locale
backend/utils/adt/pg_locale/src/lib.rs	COLLATION_CACHE	pgport	at:pg_newlocale_from_collation	leader	built only with a catcache; collid->locale map, taken by the session cleanup pg_newlocale_from_collation registers
backend/utils/adt/pg_locale/src/lib.rs	LAST_COLLATION_CACHE	pgport	key:the collation OID	leader	built only with a catcache; last-lookup memo (read BEFORE the armed collations), checked by OID, never cleared
backend/utils/adt/pg_locale/src/lib.rs	RESOLVED	claim	claim	claim	collations
backend/utils/adt/pg_locale/src/lib.rs	ARMED_COLLATIONS	claim	claim	claim	collations
backend/utils/adt/pg_locale/src/lib.rs	XFRM_SCRATCH	pgport	call	scratch	pg_strnxfrm sort-key buffer for the nondeterministic hash, cleared at each use
