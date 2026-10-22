-- DENYLIST, stored without statement text (do not train on these). The published TPC-H measurement
-- vector (22 query templates as the project's TPC-H runner issues them), kept as:
--   * one column-blind skeleton per line, as `pgo/lint-training-overlap.sh --skeletons <query file> | sort -u`
--     prints them: every table, column and alias is I, every literal is @ (rule 2, and verbatim copies);
--   * one "-- pgo-lint: exact-sha256" line per statement: the sha256 of its canonical form, as
--     `pgo/lint-training-overlap.sh --exact-hashes <query file>` prints them (rule 1, enforced on every
--     training file, oltp-* included).
-- Literals collapse in both forms, so every substitution of a template is denied. This file exists ONLY
-- so pgo/lint-training-overlap.sh can prove the training corpus is disjoint from it. Nothing in the build
-- reads it as input. Regenerate: see pgo/README.md ("Denylist sources").
-- pgo-lint: skeletons
select @ * sum ( case when I like @ then I * ( @ - I ) else @ end ) / sum ( I * ( @ - I ) ) from I , I where I = I and I > = date @ and I < cast ( @ date ) ;
select I , I , I , I , I , I , I , I from I , I , I , I , I where I = I and I = I and I = @ and I like @ and I = I and I = I and I = @ and I = ( select min ( I ) from I , I , I , I where I = I and I = I and I = I and I = I and I = @ ) order by I desc , I , I , I limit @ ;
select I , I , I , I , I , sum ( I ) from I , I , I where I in ( select I from I group by I having sum ( I ) > @ ) and I = I and I = I group by I , I , I , I , I order by I desc , I limit @ ;
select I , I , I , count ( distinct I ) from I , I where I = I and I < > @ and I not like @ and I in ( @ ) and I not in ( select I from I where I like @ ) group by I , I , I order by I desc , I , I , I ;
select I , I , I , sum ( I ) from ( select I , I , extract ( @ from I ) , I * ( @ - I ) from I , I , I , I , I , I where I = I and I = I and I = I and I = I and I = I and ( ( I = @ and I = @ ) or ( I = @ and I = @ ) ) and I between cast ( @ date ) and cast ( @ date ) ) group by I , I , I order by I , I , I ;
select I , I , sum ( I ) , sum ( I ) , sum ( I * ( @ - I ) ) , sum ( I * ( @ - I ) * ( @ + I ) ) , avg ( I ) , avg ( I ) , avg ( I ) , count ( * ) from I where I < = cast ( @ date ) group by I , I order by I , I ;
select I , I , sum ( I ) from ( select I , extract ( @ from I ) , I * ( @ - I ) - I * I from I , I , I , I , I , I where I = I and I = I and I = I and I = I and I = I and I = I and I like @ ) group by I , I order by I , I desc ;
select I , I , sum ( I * ( @ - I ) ) , I , I , I , I , I from I , I , I , I where I = I and I = I and I > = cast ( @ date ) and I < cast ( @ date ) and I = @ and I = I group by I , I , I , I , I , I , I order by I desc limit @ ;
select I , I from I , I where I in ( select I from I where I in ( select I from I where I like @ ) and I > ( select @ * sum ( I ) from I where I = I and I = I and I > = cast ( @ date ) and I < cast ( @ date ) ) ) and I = I and I = @ order by I ;
select I , count ( * ) , sum ( I ) from ( select substring ( I from @ for @ ) , I from I where substring ( I from @ for @ ) in ( @ ) and I > ( select avg ( I ) from I where I > @ and substring ( I from @ for @ ) in ( @ ) ) and not exists ( select * from I where I = I ) ) group by I order by I ;
select I , count ( * ) from ( select I , count ( I ) from I left join I on I = I and I not like @ group by I ) c_orders ( I , I ) group by I order by I desc , I desc ;
select I , count ( * ) from I , I , I , I where I = I and I = I and I = @ and I > I and exists ( select * from I where I = I and I < > I ) and not exists ( select * from I where I = I and I < > I and I > I ) and I = I and I = @ group by I order by I desc , I limit @ ;
select I , count ( * ) from I where I > = cast ( @ date ) and I < cast ( @ date ) and exists ( select * from I where I = I and I < I ) group by I order by I ;
select I , sum ( I * ( @ - I ) ) , I , I from I , I , I where I = @ and I = I and I = I and I < cast ( @ date ) and I > cast ( @ date ) group by I , I , I order by I desc , I limit @ ;
select I , sum ( I * ( @ - I ) ) from I , I , I , I , I , I where I = I and I = I and I = I and I = I and I = I and I = I and I = @ and I > = cast ( @ date ) and I < cast ( @ date ) group by I order by I desc ;
select I , sum ( I * I ) from I , I , I where I = I and I = I and I = @ group by I having sum ( I * I ) > ( select sum ( I * I ) * @ from I , I , I where I = I and I = I and I = @ ) order by I desc ;
select I , sum ( case when I = @ or I = @ then @ else @ end ) , sum ( case when I < > @ and I < > @ then @ else @ end ) from I , I where I = I and I in ( @ ) and I < I and I < I and I > = cast ( @ date ) and I < cast ( @ date ) group by I order by I ;
select I , sum ( case when I = @ then I else @ end ) / sum ( I ) from ( select extract ( @ from I ) , I * ( @ - I ) , I from I , I , I , I , I , I , I , I where I = I and I = I and I = I and I = I and I = I and I = I and I = @ and I = I and I between cast ( @ date ) and cast ( @ date ) and I = @ ) group by I order by I ;
select sum ( I ) / @ from I , I where I = I and I = @ and I = @ and I < ( select @ * avg ( I ) from I where I = I ) ;
select sum ( I * ( @ - I ) ) from I , I where ( I = I and I = @ and I in ( @ ) and I > = @ and I < = @ + @ and I between @ and @ and I in ( @ ) and I = @ ) or ( I = I and I = @ and I in ( @ ) and I > = @ and I < = @ + @ and I between @ and @ and I in ( @ ) and I = @ ) or ( I = I and I = @ and I in ( @ ) and I > = @ and I < = @ + @ and I between @ and @ and I in ( @ ) and I = @ ) ;
select sum ( I * I ) from I where I > = cast ( @ date ) and I < cast ( @ date ) and I between @ and @ and I < @ ;
with I ( select I , sum ( I * ( @ - I ) ) from I where I > = cast ( @ date ) and I < cast ( @ date ) group by I ) select I , I , I , I , I from I , I where I = I and I = ( select max ( I ) from I ) order by I ;
-- pgo-lint: exact-sha256 1197f0f65cf30319142a2338d2768a4b5b84cde83206741d0f6c14a96777a720
-- pgo-lint: exact-sha256 18cf25e69ccecae7ea1f394c97f0d1a0bf2f8abb221003b78d8e2a5ba7722a2e
-- pgo-lint: exact-sha256 2eb43845e665589e06305b8c87ecb87332df37214bef1852dda8c035cedaa290
-- pgo-lint: exact-sha256 32f274a4397839e64970b25dda56df418c7056c2577cab3542bb0fbcd9faf216
-- pgo-lint: exact-sha256 34f4d0094acae46744fee2264eb02840e98cce430aea8dbd942ab9059768c57d
-- pgo-lint: exact-sha256 529f48302a3c8a4797a60116cb75f39d862ee251c80cd53554f445accb8584f9
-- pgo-lint: exact-sha256 6a4973f589cf5b993cf70da164709cf75924593ac2cdf6c9a9096b05b8f6a35e
-- pgo-lint: exact-sha256 847a58ed6fa8854185d7f1990851f03f599df1e225a3e38f8851a6cd292a2e7a
-- pgo-lint: exact-sha256 8748858f0e64cbab9d8b237578a7c0f56f33b76132980d6e5edf62739c0fd512
-- pgo-lint: exact-sha256 891dc37f18ca6d75557a7f36a4f67bd9154325746ce4efad5342b798752b9f44
-- pgo-lint: exact-sha256 8fde3ef624b78c05dc95e4e019dde62cdb1ca7327bf09e348dc131f601798574
-- pgo-lint: exact-sha256 95d525f4abd375277ff6825b580b4b9d8dde0cf350a56657d7f21aa04ca89109
-- pgo-lint: exact-sha256 9ca994502b5db0def1ae9fdba609c2fcc583f98722431f28c7da753b31988e13
-- pgo-lint: exact-sha256 a05824972e9721ab32227751b88250171698d845055677b5229e0ba2c7652827
-- pgo-lint: exact-sha256 a7c31b4e24cb34f38140bad8e5c7b94560a80698cd00eefc1b5046eeae1c68b5
-- pgo-lint: exact-sha256 a8fbe70c1c23ded3bdb9b62baaada8600051f0cbc4c87f7b6c771b4427df302c
-- pgo-lint: exact-sha256 ae8944880789035a47d65dedbdcd5157120eb3bd35fb141fb77ce8b5e8979857
-- pgo-lint: exact-sha256 b40c4ef3a82ad1ae8254bf4d44ab37f4c84c1bece3ad42f4e6d7040e2713592a
-- pgo-lint: exact-sha256 b57cc4e631a33b530c07fa68217d9c81921cba95e5b19ba505b1e0edd430dbc9
-- pgo-lint: exact-sha256 e75215fdc749ce77e8684f181b3e079179c894379ba0069f621443f799564d02
-- pgo-lint: exact-sha256 fa7085ffb2fbe346098b113b838f0964e7c1e3532b0232184ac31ea763b20415
-- pgo-lint: exact-sha256 fec7e371714554f4188ad95baf81b3098b468b37fc4960487bcb1ee5c5c72ae9
