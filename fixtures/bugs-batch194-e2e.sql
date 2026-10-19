-- bugs/batch-194-backend-commands-tsearchcmds: SQL-level regressions, expected captured from C 18.6.
\set VERBOSITY verbose

-- regression / regression / regression / regression /
-- regression: the init method reads an option's value only when it asks
-- for it (defGetString), so an unknown valueless option is 22023, a known one 42601, and
-- a valueless boolean (defGetBoolean) means true.
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = simple, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = simple, stopwords);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = simple, accept, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = simple, accept);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = simple, stopwords = 'english', stopwords);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = snowball, language = english, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = snowball, language);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = synonym, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = synonym, synonyms = synonym_sample, casesensitive);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = thesaurus, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = ispell, bogus);
CREATE EXTENSION unaccent;
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = unaccent, bogus);
CREATE TEXT SEARCH DICTIONARY b194_d (TEMPLATE = unaccent, rules);
CREATE TEXT SEARCH DICTIONARY b194_d2 (TEMPLATE = simple);
ALTER TEXT SEARCH DICTIONARY b194_d2 (bogus);
ALTER TEXT SEARCH DICTIONARY b194_d2 (accept, bogus);
ALTER TEXT SEARCH DICTIONARY b194_d2 (stopwords);
SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'b194_d2';

-- regression: an internal-language alias of void_send as a template
-- init method runs with an active allocation context (OidFunctionCall1).
CREATE FUNCTION b194_init(internal) RETURNS internal LANGUAGE internal AS 'void_send';
CREATE TEXT SEARCH TEMPLATE b194_t (INIT = b194_init, LEXIZE = dsimple_lexize);
CREATE TEXT SEARCH DICTIONARY b194_d3 (TEMPLATE = b194_t, anything = 1);
SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'b194_d3';
ALTER TEXT SEARCH DICTIONARY b194_d3 (other = 2);
SELECT dictinitoption FROM pg_ts_dict WHERE dictname = 'b194_d3';
