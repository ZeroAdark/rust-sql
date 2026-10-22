-- DENYLIST, stored as column-blind skeletons (do not train on these shapes).
-- One line per distinct skeleton of the published single-table analytical benchmark vector this
-- project has reported numbers on, as `pgo/lint-training-overlap.sh --skeletons <query file> | sort -u`
-- prints them: every table, column and alias is I, every literal is @, so the file carries no
-- statement text. The lint compares training statements against these skeletons (rule 2), which
-- denies exact copies (equal statements have equal skeletons) and copies with renamed columns.
-- To check the corpus against any other query file, see pgo/README.md ("Checking against your own
-- queries"). Nothing in the build reads this file as training input.
-- pgo-lint: skeletons
select @ , I , count ( * ) from I group by @ , I order by I desc limit @ ;
select * from I where I like @ order by I limit @ ;
select avg ( I ) from I ;
select count ( * ) from I ;
select count ( * ) from I where I < > @ ;
select count ( * ) from I where I like @ ;
select count ( distinct I ) from I ;
select date_trunc ( @ , I ) , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ and I = @ group by date_trunc ( @ , I ) order by date_trunc ( @ , I ) limit @ offset @ ;
select I , avg ( octet_length ( I ) ) , count ( * ) from I where I < > @ group by I having count ( * ) > @ order by I desc limit @ ;
select I , count ( * ) from I group by I order by count ( * ) desc limit @ ;
select I , count ( * ) from I group by I order by I desc limit @ ;
select I , count ( * ) from I where I < > @ group by I order by count ( * ) desc ;
select I , count ( * ) from I where I < > @ group by I order by I desc limit @ ;
select I , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ and I < > @ and I = @ group by I order by I desc limit @ offset @ ;
select I , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ and I = @ and I < > @ group by I order by I desc limit @ ;
select I , count ( distinct I ) from I group by I order by I desc limit @ ;
select I , count ( distinct I ) from I where I < > @ group by I order by I desc limit @ ;
select I , extract ( @ from I ) , I , count ( * ) from I group by I , I , I order by count ( * ) desc limit @ ;
select I , I - @ , I - @ , I - @ , count ( * ) from I group by I , I - @ , I - @ , I - @ order by I desc limit @ ;
select I , I , count ( * ) , sum ( I ) , avg ( I ) from I group by I , I order by I desc limit @ ;
select I , I , count ( * ) , sum ( I ) , avg ( I ) from I where I < > @ group by I , I order by I desc limit @ ;
select I , I , count ( * ) from I group by I , I limit @ ;
select I , I , count ( * ) from I group by I , I order by count ( * ) desc limit @ ;
select I , I , count ( * ) from I where I < > @ group by I , I order by I desc limit @ ;
select I , I , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ and I = @ and I = @ group by I , I order by I desc limit @ offset @ ;
select I , I , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ and I in ( @ ) and I = @ group by I , I order by I desc limit @ offset @ ;
select I , I , count ( distinct I ) from I where I < > @ group by I , I order by I desc limit @ ;
select I , I , I , case when ( I = @ and I = @ ) then I else @ end , I , count ( * ) from I where I = @ and I > = @ and I < = @ and I = @ group by I , I , I , I , I order by I desc limit @ offset @ ;
select I , min ( I ) , count ( * ) from I where I like @ and I < > @ group by I order by I desc limit @ ;
select I , min ( I ) , min ( I ) , count ( * ) , count ( distinct I ) from I where I like @ and I not like @ and I < > @ group by I order by I desc limit @ ;
select I , sum ( I ) , count ( * ) , avg ( I ) , count ( distinct I ) from I group by I order by I desc limit @ ;
select I from I where I < > @ order by I , I limit @ ;
select I from I where I < > @ order by I limit @ ;
select I from I where I = @ ;
select min ( I ) , max ( I ) from I ;
select regexp_replace ( I , @ , @ ) , avg ( octet_length ( I ) ) , count ( * ) , min ( I ) from I where I < > @ group by I having count ( * ) > @ order by I desc limit @ ;
select sum ( I ) , count ( * ) , avg ( I ) from I ;
select sum ( I ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) , sum ( I + @ ) from I ;
