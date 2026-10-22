backend/utils/adt/pg_locale/src/setup.rs	DATABASE_CTYPE_IS_C	claim	claim	claim	collations
backend/utils/adt/pg_locale/src/setup.rs	LOCALE_MONETARY	claim	claim	claim	gucs
backend/utils/adt/pg_locale/src/setup.rs	LOCALE_NUMERIC	claim	claim	claim	gucs
backend/utils/adt/pg_locale/src/setup.rs	LOCALE_TIME	claim	claim	claim	gucs
backend/utils/adt/pg_locale/src/setup.rs	LOCALE_MESSAGES	claim	claim	claim	gucs
backend/utils/adt/pg_locale/src/setup.rs	ICU_VALIDATION_LEVEL	claim	claim	claim	gucs
backend/utils/adt/pg_locale/src/setup.rs	THREAD_LOCALES	claim	claim	claim	collations
backend/utils/adt/pg_locale/src/setup.rs	CURRENT_LOCALE_CONV	pgport	key:lc_monetary and lc_numeric (cleared by their assign hooks and variable writes)	cache	cleared by the lc_monetary / lc_numeric hooks the gucs member runs (D3919); the converted localeconv
