SELECT * FROM JSON_TABLE(jsonb '1', '$' PASSING generate_series(1,2) AS x COLUMNS (a int PATH '$')) jt;
SELECT * FROM JSON_TABLE(generate_series(1,2)::text::jsonb, '$' COLUMNS (a int PATH '$')) jt;
SELECT * FROM JSON_TABLE(jsonb '1', '$' COLUMNS (a int PATH '$' DEFAULT generate_series(1,2) ON ERROR)) jt;
