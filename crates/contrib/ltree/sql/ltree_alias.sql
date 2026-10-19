CREATE EXTENSION ltree;

-- text2ltree and the text concatenation operators go through
-- text_to_cstring: the parser sees the text only up to its first NUL.
CREATE FUNCTION text2ltree_alias(bytea) RETURNS ltree
  LANGUAGE C STRICT AS '$libdir/ltree', 'text2ltree';
CREATE FUNCTION ltree_addtext_alias(ltree, bytea) RETURNS ltree
  LANGUAGE C STRICT AS '$libdir/ltree', 'ltree_addtext';
CREATE FUNCTION ltree_textadd_alias(bytea, ltree) RETURNS ltree
  LANGUAGE C STRICT AS '$libdir/ltree', 'ltree_textadd';
SELECT text2ltree_alias('a.b\000c'::bytea);
SELECT ltree_addtext_alias('x'::ltree, 'a.b\000c'::bytea);
SELECT ltree_textadd_alias('a.b\000c'::bytea, 'x'::ltree);
SELECT text2ltree_alias('a.b.c'::bytea);
SELECT text2ltree_alias('\000a.b.c'::bytea);
