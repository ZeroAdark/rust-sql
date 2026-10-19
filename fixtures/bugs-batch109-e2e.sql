-- bugs/batch-109-backend-tsearch: SQL-level regressions, expected captured from C 18.6.
\set VERBOSITY verbose

-- regression: ts_parse / ts_token_type form their rows through the caller's
-- result descriptor (prs_setup_firstcall / tt_setup_firstcall: get_call_result_type +
-- TupleDescGetAttInMetadata + BuildTupleFromCStrings), so an internal-language alias
-- with another column definition list converts through those columns' input functions.
CREATE FUNCTION b109_parse(text, text) RETURNS SETOF record LANGUAGE internal STRICT AS 'ts_parse_byname';
SELECT * FROM b109_parse('default', '123') AS t(tokid integer, token integer);
SELECT * FROM b109_parse('default', '123 x') AS t(tokid text, token text);
SELECT * FROM b109_parse('default', '12 abc') AS t(tokid int2, token varchar(2));
SELECT * FROM b109_parse('default', 'x') AS t(tokid integer, token integer);
CREATE FUNCTION b109_parse_id(oid, text) RETURNS SETOF record LANGUAGE internal STRICT AS 'ts_parse_byid';
SELECT * FROM b109_parse_id(3722, '7 x') AS t(tokid numeric, token name);
CREATE FUNCTION b109_tt(text) RETURNS SETOF record LANGUAGE internal STRICT AS 'ts_token_type_byname';
SELECT * FROM b109_tt('default') AS t(tokid int8, alias name, description varchar) WHERE tokid < 4;
CREATE FUNCTION b109_tt_id(oid) RETURNS SETOF record LANGUAGE internal STRICT AS 'ts_token_type_byid';
SELECT * FROM b109_tt_id(3722) AS t(tokid text, alias text, description text) WHERE tokid = '22';
SELECT * FROM b109_tt_id(3722) AS t(tokid integer, alias varchar(3), description text) WHERE tokid = 22;
-- the stock signatures resolve their OUT parameters the same way
SELECT * FROM ts_parse('default', 'a 1');
SELECT tokid, alias FROM ts_token_type('default') WHERE tokid > 20;
SELECT (t).tokid, (t).token FROM ts_parse('default', 'z') AS t;
SELECT ts_parse('default', 'q');
SELECT ts_token_type('default') LIMIT 2;
