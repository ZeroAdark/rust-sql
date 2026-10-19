SELECT 1;
DO $$ BEGIN RAISE WARNING 'b230ext script warning'; END $$;
CREATE TABLE b230ext_t(a int);
