CREATE FUNCTION p158_dump(a bigint) RETURNS bigint LANGUAGE plpgsql AS $$ #option dump BEGIN RETURN 1; END $$;
SELECT p158_dump(1);
CREATE FUNCTION p158_loop(a bigint) RETURNS bigint LANGUAGE plpgsql AS $$ #variable_conflict error #option dump DECLARE s bigint := 0; BEGIN FOR i IN 1..a LOOP s := s + i; END LOOP; RETURN s; END $$;
SELECT p158_loop(4);
DO $$ #option dump BEGIN PERFORM 1; END $$;
CREATE FUNCTION p158_control() RETURNS int LANGUAGE plpgsql AS $$ #print_strict_params on #variable_conflict use_column BEGIN RETURN 2; END $$;
SELECT p158_control();
