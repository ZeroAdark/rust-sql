# gravity-asm corpus: kernel par_hashagg
# entry: gravity_kernels::par_hashagg::par_hashagg
# symbol: gravity_kernels::par_hashagg::par_hashagg
# arch arm64 Darwin; rustc 1.96.0 (ac68faa20 2026-05-25); profile dist (lto fat, codegen-units 1); 2026-09-16T15:49Z; af7d08b382e
# function: 9773 instructions, 2237 blocks, 172 loops, 141 innermost: 129 hot, 12 window, 0 other
# hot LBB94_6: 4 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_14: 5 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_29: 11 instructions in 2 blocks, loads 4 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_42: 9 instructions in 2 blocks, loads 1 (gather 1), stores 2, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_67: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_85: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_103: 11 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_119: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_135: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_151: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_167: 10 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_183: 11 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_199: 11 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_214: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_232: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_255: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_266: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_293: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_316: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_335: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_351: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_362: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_380: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_403: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_414: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_441: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_464: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_483: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_499: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_510: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_528: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_551: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_562: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_589: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_612: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_631: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_647: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_658: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_676: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_699: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_710: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_737: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_760: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_779: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_795: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_806: 19 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_824: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_847: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_858: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_885: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_908: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_927: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_943: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_954: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_972: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_995: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1006: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1033: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1056: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1075: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1091: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1102: 18 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1120: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1143: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1154: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1181: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1204: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1223: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1239: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1250: 19 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1268: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1291: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1302: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1329: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1352: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1371: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1387: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1398: 19 instructions in 8 blocks, loads 3 (gather 3), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1416: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1439: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1450: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1477: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1500: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1519: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1535: 9 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1594: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1597: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1602: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1605: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1619: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1622: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1627: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1630: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1644: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1647: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1652: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1655: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1669: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1672: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1677: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1680: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1694: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1697: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1702: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1705: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1719: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1722: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1727: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1730: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1744: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1747: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1752: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1755: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1769: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1772: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1777: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1780: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1794: 11 instructions in 1 blocks, loads 4 (gather 0), stores 4, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1797: 4 instructions in 1 blocks, loads 1 (gather 0), stores 1, tests 0, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1802: 9 instructions in 2 blocks, loads 2 (gather 2), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1805: 8 instructions in 2 blocks, loads 1 (gather 1), stores 0, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1842: 8 instructions in 2 blocks, loads 1 (gather 0), stores 0, tests 2, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1881: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1900: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1911: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1941: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1960: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1978: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# hot LBB94_1994: 8 instructions in 1 blocks, loads 1 (gather 1), stores 1, tests 1, flag-or 0, calls 0 (indirect 0, cold 0), stack 0
# window LBB94_50: 18 instructions in 5 blocks, loads 7 (gather 1), stores 0, tests 2, flag-or 0, calls 1 (indirect 1, cold 0), stack 2
# window LBB94_297: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_445: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_593: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_741: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_889: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_1037: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_1185: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_1333: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_1481: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_1945: 11 instructions in 2 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 1
# window LBB94_2006: 11 instructions in 3 blocks, loads 2 (gather 0), stores 0, tests 0, flag-or 0, calls 2 (indirect 1, cold 0), stack 3
## hot LBB94_6 (blocks [9, 10])
LBB94_6:
	ldrb	w10, [x9, x8]                       ; gather
	cbz	w10, LBB94_17                        ; branch
	adds	x8, x8, #1                          ; alu
	b.ne	LBB94_6                             ; branch
## hot LBB94_14 (blocks [17, 18])
LBB94_14:
	ldrb	w12, [x9, x11]                      ; gather
	cbz	w12, LBB94_16                        ; branch
	add	x11, x11, #1                         ; alu
	cmp	x10, x11                             ; test
	b.ne	LBB94_14                            ; branch
## hot LBB94_29 (blocks [39, 40])
LBB94_29:
	cmp	x10, x11                             ; test
	b.eq	LBB94_47                            ; branch
	add	x8, x11, #1                          ; alu
	ldr	w12, [x28, #60]                      ; load
	ldrb	w13, [x28, #42]                     ; load
	madd	x11, x11, x13, x28                  ; alu
	add	x12, x11, x12                        ; alu
	ldrb	w11, [x28, #44]                     ; load
	ldr	x13, [x12, x11]                      ; gather
	mov	x11, x8                              ; alu
	cbz	x13, LBB94_29                        ; branch
## hot LBB94_42 (blocks [53, 54])
LBB94_42:
	sub	x12, x9, #1                          ; alu
	lsr	x12, x12, #1                         ; alu
	ldr	x13, [x27, x12, lsl #3]              ; gather
	cmp	x13, x10                             ; test
	b.ge	LBB94_27                            ; branch
	str	x13, [x27, x9, lsl #3]               ; store
	str	x10, [x27, x12, lsl #3]              ; store
	mov	x9, x12                              ; alu
	cbnz	x12, LBB94_42                       ; branch
## hot LBB94_67 (blocks [81, 82])
LBB94_67:
	add	x23, x24, #1                         ; alu
	ldrsw	x1, [x26, x24, lsl #2]             ; gather
	ldrsw	x2, [x20, x24, lsl #2]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_69                            ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_67                            ; branch
## hot LBB94_85 (blocks [102, 103])
LBB94_85:
	add	x23, x24, #1                         ; alu
	ldrsh	x1, [x26, x24, lsl #1]             ; gather
	ldrsw	x2, [x20, x24, lsl #2]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_87                            ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_85                            ; branch
## hot LBB94_103 (blocks [123, 124])
LBB94_103:
	add	x23, x24, #1                         ; alu
	lsl	x10, x24, #3                         ; alu
	ldrsw	x1, [x20, x10]                     ; gather
	ldrsw	x2, [x27, x24, lsl #2]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_105                           ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_103                           ; branch
## hot LBB94_119 (blocks [142, 143])
LBB94_119:
	add	x23, x24, #1                         ; alu
	ldrsw	x1, [x26, x24, lsl #2]             ; gather
	ldrsh	x2, [x20, x24, lsl #1]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_121                           ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_119                           ; branch
## hot LBB94_135 (blocks [161, 162])
LBB94_135:
	add	x23, x24, #1                         ; alu
	ldrsh	x1, [x26, x24, lsl #1]             ; gather
	ldrsh	x2, [x20, x24, lsl #1]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_137                           ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_135                           ; branch
## hot LBB94_151 (blocks [180, 181])
LBB94_151:
	add	x23, x24, #1                         ; alu
	ldrsw	x1, [x26, x24, lsl #2]             ; gather
	ldr	x2, [x20, x24, lsl #3]               ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_153                           ; branch
	add	x22, x2, x22                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_151                           ; branch
## hot LBB94_167 (blocks [199, 200])
LBB94_167:
	add	x23, x24, #1                         ; alu
	ldrsh	x1, [x26, x24, lsl #1]             ; gather
	ldr	x2, [x20, x24, lsl #3]               ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_169                           ; branch
	add	x22, x2, x22                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_167                           ; branch
## hot LBB94_183 (blocks [218, 219])
LBB94_183:
	add	x23, x24, #1                         ; alu
	lsl	x10, x24, #3                         ; alu
	ldrsw	x1, [x20, x10]                     ; gather
	ldrsh	x2, [x27, x24, lsl #1]             ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_185                           ; branch
	add	x22, x22, x2                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_183                           ; branch
## hot LBB94_199 (blocks [237, 238])
LBB94_199:
	add	x23, x24, #1                         ; alu
	lsl	x10, x24, #3                         ; alu
	ldrsw	x1, [x26, x10]                     ; gather
	ldr	x2, [x20, x10]                       ; gather
	cmp	x9, x1                               ; test
	b.ne	LBB94_201                           ; branch
	add	x22, x2, x22                         ; alu
	add	x8, x8, #1                           ; alu
	mov	x24, x23                             ; alu
	cmp	x23, x21                             ; test
	b.lo	LBB94_199                           ; branch
## hot LBB94_214 (blocks [253, 254, 255, 256, 257, 258, 259, 260])
LBB94_212:
	ldrsw	x27, [x28, x10, lsl #2]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_218                           ; branch
LBB94_213:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_214:
	cbz	x8, LBB94_216                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_217                      ; branch
	b	LBB94_212                              ; branch
LBB94_216:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_212                       ; branch
LBB94_217:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_212                  ; branch
	b	LBB94_213                              ; branch
## hot LBB94_232 (blocks [275])
LBB94_232:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_232                           ; branch
## hot LBB94_255 (blocks [298])
LBB94_255:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_255                           ; branch
## hot LBB94_266 (blocks [310])
LBB94_266:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_266                           ; branch
## hot LBB94_293 (blocks [338])
LBB94_293:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_293                           ; branch
## hot LBB94_316 (blocks [363])
LBB94_316:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_316                           ; branch
## hot LBB94_335 (blocks [385])
LBB94_335:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_335                           ; branch
## hot LBB94_351 (blocks [402])
LBB94_351:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_351                           ; branch
## hot LBB94_362 (blocks [411, 412, 413, 414, 415, 416, 417, 418])
LBB94_360:
	ldrsh	x27, [x28, x10, lsl #1]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_366                           ; branch
LBB94_361:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_362:
	cbz	x8, LBB94_364                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_365                      ; branch
	b	LBB94_360                              ; branch
LBB94_364:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_360                       ; branch
LBB94_365:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_360                  ; branch
	b	LBB94_361                              ; branch
## hot LBB94_380 (blocks [433])
LBB94_380:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_380                           ; branch
## hot LBB94_403 (blocks [456])
LBB94_403:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_403                           ; branch
## hot LBB94_414 (blocks [468])
LBB94_414:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_414                           ; branch
## hot LBB94_441 (blocks [496])
LBB94_441:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_441                           ; branch
## hot LBB94_464 (blocks [520])
LBB94_464:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_464                           ; branch
## hot LBB94_483 (blocks [541])
LBB94_483:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_483                           ; branch
## hot LBB94_499 (blocks [558])
LBB94_499:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_499                           ; branch
## hot LBB94_510 (blocks [567, 568, 569, 570, 571, 572, 573, 574])
LBB94_508:
	ldrsw	x27, [x28, x10, lsl #2]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_514                           ; branch
LBB94_509:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_510:
	cbz	x8, LBB94_512                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_513                      ; branch
	b	LBB94_508                              ; branch
LBB94_512:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_508                       ; branch
LBB94_513:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_508                  ; branch
	b	LBB94_509                              ; branch
## hot LBB94_528 (blocks [589])
LBB94_528:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_528                           ; branch
## hot LBB94_551 (blocks [612])
LBB94_551:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_551                           ; branch
## hot LBB94_562 (blocks [624])
LBB94_562:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_562                           ; branch
## hot LBB94_589 (blocks [652])
LBB94_589:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_589                           ; branch
## hot LBB94_612 (blocks [676])
LBB94_612:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_612                           ; branch
## hot LBB94_631 (blocks [697])
LBB94_631:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_631                           ; branch
## hot LBB94_647 (blocks [714])
LBB94_647:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_647                           ; branch
## hot LBB94_658 (blocks [723, 724, 725, 726, 727, 728, 729, 730])
LBB94_656:
	ldrsh	x27, [x26, x10, lsl #1]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_662                           ; branch
LBB94_657:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_658:
	cbz	x8, LBB94_660                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x28, LBB94_661                      ; branch
	b	LBB94_656                              ; branch
LBB94_660:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x28, LBB94_656                       ; branch
LBB94_661:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x28, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_656                  ; branch
	b	LBB94_657                              ; branch
## hot LBB94_676 (blocks [745])
LBB94_676:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_676                           ; branch
## hot LBB94_699 (blocks [768])
LBB94_699:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_699                           ; branch
## hot LBB94_710 (blocks [780])
LBB94_710:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_710                           ; branch
## hot LBB94_737 (blocks [808])
LBB94_737:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_737                           ; branch
## hot LBB94_760 (blocks [832])
LBB94_760:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_760                           ; branch
## hot LBB94_779 (blocks [853])
LBB94_779:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_779                           ; branch
## hot LBB94_795 (blocks [870])
LBB94_795:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_795                           ; branch
## hot LBB94_806 (blocks [879, 880, 881, 882, 883, 884, 885, 886])
LBB94_804:
	lsl	x11, x10, #3                         ; alu
	ldrsw	x27, [x28, x11]                    ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_810                           ; branch
LBB94_805:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_806:
	cbz	x8, LBB94_808                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_809                      ; branch
	b	LBB94_804                              ; branch
LBB94_808:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_804                       ; branch
LBB94_809:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_804                  ; branch
	b	LBB94_805                              ; branch
## hot LBB94_824 (blocks [901])
LBB94_824:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_824                           ; branch
## hot LBB94_847 (blocks [924])
LBB94_847:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_847                           ; branch
## hot LBB94_858 (blocks [936])
LBB94_858:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_858                           ; branch
## hot LBB94_885 (blocks [964])
LBB94_885:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x1, x11]                       ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_885                           ; branch
## hot LBB94_908 (blocks [988])
LBB94_908:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_908                           ; branch
## hot LBB94_927 (blocks [1009])
LBB94_927:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_927                           ; branch
## hot LBB94_943 (blocks [1026])
LBB94_943:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_943                           ; branch
## hot LBB94_954 (blocks [1035, 1036, 1037, 1038, 1039, 1040, 1041, 1042])
LBB94_952:
	ldrsw	x27, [x28, x10, lsl #2]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_958                           ; branch
LBB94_953:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_954:
	cbz	x8, LBB94_956                        ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_957                      ; branch
	b	LBB94_952                              ; branch
LBB94_956:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_952                       ; branch
LBB94_957:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_952                  ; branch
	b	LBB94_953                              ; branch
## hot LBB94_972 (blocks [1057])
LBB94_972:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_972                           ; branch
## hot LBB94_995 (blocks [1080])
LBB94_995:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_995                           ; branch
## hot LBB94_1006 (blocks [1092])
LBB94_1006:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1006                          ; branch
## hot LBB94_1033 (blocks [1120])
LBB94_1033:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1033                          ; branch
## hot LBB94_1056 (blocks [1144])
LBB94_1056:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1056                          ; branch
## hot LBB94_1075 (blocks [1165])
LBB94_1075:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1075                          ; branch
## hot LBB94_1091 (blocks [1182])
LBB94_1091:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1091                          ; branch
## hot LBB94_1102 (blocks [1191, 1192, 1193, 1194, 1195, 1196, 1197, 1198])
LBB94_1100:
	ldrsh	x27, [x28, x10, lsl #1]            ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_1106                          ; branch
LBB94_1101:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_1102:
	cbz	x8, LBB94_1104                       ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_1105                     ; branch
	b	LBB94_1100                             ; branch
LBB94_1104:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_1100                      ; branch
LBB94_1105:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_1100                 ; branch
	b	LBB94_1101                             ; branch
## hot LBB94_1120 (blocks [1213])
LBB94_1120:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1120                          ; branch
## hot LBB94_1143 (blocks [1236])
LBB94_1143:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1143                          ; branch
## hot LBB94_1154 (blocks [1248])
LBB94_1154:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1154                          ; branch
## hot LBB94_1181 (blocks [1276])
LBB94_1181:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x21, x11]                      ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1181                          ; branch
## hot LBB94_1204 (blocks [1300])
LBB94_1204:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1204                          ; branch
## hot LBB94_1223 (blocks [1321])
LBB94_1223:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1223                          ; branch
## hot LBB94_1239 (blocks [1338])
LBB94_1239:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x21, x10]                      ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1239                          ; branch
## hot LBB94_1250 (blocks [1347, 1348, 1349, 1350, 1351, 1352, 1353, 1354])
LBB94_1248:
	lsl	x11, x10, #3                         ; alu
	ldrsw	x27, [x28, x11]                    ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_1254                          ; branch
LBB94_1249:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_1250:
	cbz	x8, LBB94_1252                       ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_1253                     ; branch
	b	LBB94_1248                             ; branch
LBB94_1252:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_1248                      ; branch
LBB94_1253:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_1248                 ; branch
	b	LBB94_1249                             ; branch
## hot LBB94_1268 (blocks [1369])
LBB94_1268:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1268                          ; branch
## hot LBB94_1291 (blocks [1392])
LBB94_1291:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1291                          ; branch
## hot LBB94_1302 (blocks [1404])
LBB94_1302:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1302                          ; branch
## hot LBB94_1329 (blocks [1432])
LBB94_1329:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x1, x11]                       ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1329                          ; branch
## hot LBB94_1352 (blocks [1456])
LBB94_1352:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1352                          ; branch
## hot LBB94_1371 (blocks [1477])
LBB94_1371:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1371                          ; branch
## hot LBB94_1387 (blocks [1494])
LBB94_1387:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1387                          ; branch
## hot LBB94_1398 (blocks [1503, 1504, 1505, 1506, 1507, 1508, 1509, 1510])
LBB94_1396:
	lsl	x11, x10, #3                         ; alu
	ldrsw	x27, [x28, x11]                    ; gather
	cmp	x9, x27                              ; test
	b.ge	LBB94_1402                          ; branch
LBB94_1397:
	cmp	x22, x24                             ; test
	b.hs	LBB94_1591                          ; branch
LBB94_1398:
	cbz	x8, LBB94_1400                       ; branch
	ldr	w10, [x8, x22, lsl #2]               ; gather
	add	x22, x22, #1                         ; alu
	cbnz	x26, LBB94_1401                     ; branch
	b	LBB94_1396                             ; branch
LBB94_1400:
	mov	w10, w22                             ; alu
	add	x22, x22, #1                         ; alu
	cbz	x26, LBB94_1396                      ; branch
LBB94_1401:
	lsr	x11, x10, #6                         ; alu
	ldr	x11, [x26, x11, lsl #3]              ; gather
	lsr	x11, x11, x10                        ; alu
	tbnz	w11, #0, LBB94_1396                 ; branch
	b	LBB94_1397                             ; branch
## hot LBB94_1416 (blocks [1525])
LBB94_1416:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1416                          ; branch
## hot LBB94_1439 (blocks [1548])
LBB94_1439:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1439                          ; branch
## hot LBB94_1450 (blocks [1560])
LBB94_1450:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1450                          ; branch
## hot LBB94_1477 (blocks [1588])
LBB94_1477:
	rbit	w11, w9                             ; alu
	clz	w11, w11                             ; alu
	ldrb	w11, [x10, w11, uxtw]               ; gather
	str	x13, [x1, x11]                       ; store
	sub	w11, w9, #1                          ; alu
	and	w9, w11, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1477                          ; branch
## hot LBB94_1500 (blocks [1612])
LBB94_1500:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1500                          ; branch
## hot LBB94_1519 (blocks [1633])
LBB94_1519:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1519                          ; branch
## hot LBB94_1535 (blocks [1650])
LBB94_1535:
	rbit	w10, w9                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x8, w10, uxtw]                ; gather
	mov	x11, #-1                             ; alu
	str	x11, [x1, x10]                       ; store
	sub	w10, w9, #1                          ; alu
	and	w9, w10, w9                          ; alu
	tst	w9, #0xff                            ; test
	b.ne	LBB94_1535                          ; branch
## hot LBB94_1594 (blocks [1709])
LBB94_1594:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1594                          ; branch
## hot LBB94_1597 (blocks [1712])
LBB94_1597:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1597                          ; branch
## hot LBB94_1602 (blocks [1717, 1718])
LBB94_1602:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1602                  ; branch
## hot LBB94_1605 (blocks [1720, 1721])
LBB94_1605:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1605                  ; branch
## hot LBB94_1619 (blocks [1735])
LBB94_1619:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1619                          ; branch
## hot LBB94_1622 (blocks [1738])
LBB94_1622:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1622                          ; branch
## hot LBB94_1627 (blocks [1743, 1744])
LBB94_1627:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1627                  ; branch
## hot LBB94_1630 (blocks [1746, 1747])
LBB94_1630:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1630                  ; branch
## hot LBB94_1644 (blocks [1761])
LBB94_1644:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1644                          ; branch
## hot LBB94_1647 (blocks [1764])
LBB94_1647:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1647                          ; branch
## hot LBB94_1652 (blocks [1769, 1770])
LBB94_1652:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1652                  ; branch
## hot LBB94_1655 (blocks [1772, 1773])
LBB94_1655:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1655                  ; branch
## hot LBB94_1669 (blocks [1787])
LBB94_1669:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1669                          ; branch
## hot LBB94_1672 (blocks [1790])
LBB94_1672:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1672                          ; branch
## hot LBB94_1677 (blocks [1795, 1796])
LBB94_1677:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1677                  ; branch
## hot LBB94_1680 (blocks [1798, 1799])
LBB94_1680:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1680                  ; branch
## hot LBB94_1694 (blocks [1813])
LBB94_1694:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1694                          ; branch
## hot LBB94_1697 (blocks [1816])
LBB94_1697:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1697                          ; branch
## hot LBB94_1702 (blocks [1821, 1822])
LBB94_1702:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1702                  ; branch
## hot LBB94_1705 (blocks [1824, 1825])
LBB94_1705:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1705                  ; branch
## hot LBB94_1719 (blocks [1839])
LBB94_1719:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1719                          ; branch
## hot LBB94_1722 (blocks [1842])
LBB94_1722:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1722                          ; branch
## hot LBB94_1727 (blocks [1847, 1848])
LBB94_1727:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1727                  ; branch
## hot LBB94_1730 (blocks [1850, 1851])
LBB94_1730:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1730                  ; branch
## hot LBB94_1744 (blocks [1865])
LBB94_1744:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1744                          ; branch
## hot LBB94_1747 (blocks [1868])
LBB94_1747:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1747                          ; branch
## hot LBB94_1752 (blocks [1873, 1874])
LBB94_1752:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1752                  ; branch
## hot LBB94_1755 (blocks [1876, 1877])
LBB94_1755:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1755                  ; branch
## hot LBB94_1769 (blocks [1891])
LBB94_1769:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1769                          ; branch
## hot LBB94_1772 (blocks [1894])
LBB94_1772:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1772                          ; branch
## hot LBB94_1777 (blocks [1899, 1900])
LBB94_1777:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1777                  ; branch
## hot LBB94_1780 (blocks [1902, 1903])
LBB94_1780:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1780                  ; branch
## hot LBB94_1794 (blocks [1917])
LBB94_1794:
	ldur	x11, [x9, #-48]                     ; load
	ldur	x12, [x9, #-24]                     ; load
	ldr	x13, [x9]                            ; load
	ldr	x14, [x9, #24]                       ; load
	stur	x11, [x9, #-40]                     ; store
	stur	x12, [x9, #-16]                     ; store
	str	x13, [x9, #8]                        ; store
	str	x14, [x9, #32]                       ; store
	add	x9, x9, #96                          ; alu
	subs	x10, x10, #4                        ; alu
	b.ne	LBB94_1794                          ; branch
## hot LBB94_1797 (blocks [1920])
LBB94_1797:
	ldur	x10, [x9, #-8]                      ; load
	str	x10, [x9], #24                       ; store
	subs	x8, x8, #1                          ; alu
	b.ne	LBB94_1797                          ; branch
## hot LBB94_1802 (blocks [1925, 1926])
LBB94_1802:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	ldr	w8, [x22, x10, lsl #2]               ; gather
	add	x28, x10, #1                         ; alu
	lsr	x10, x8, #6                          ; alu
	ldr	x10, [x23, x10, lsl #3]              ; gather
	lsr	x11, x10, x8                         ; alu
	mov	x10, x28                             ; alu
	tbz	w11, #0, LBB94_1802                  ; branch
## hot LBB94_1805 (blocks [1928, 1929])
LBB94_1805:
	cmp	x9, x10                              ; test
	b.eq	LBB94_1590                          ; branch
	mov	x8, x10                              ; alu
	add	x10, x10, #1                         ; alu
	lsr	x11, x8, #6                          ; alu
	ldr	x11, [x23, x11, lsl #3]              ; gather
	lsr	x11, x11, x8                         ; alu
	tbz	w11, #0, LBB94_1805                  ; branch
## hot LBB94_1842 (blocks [1992, 1993])
LBB94_1842:
	cmn	x9, #1                               ; test
	b.eq	LBB94_1863                          ; branch
	add	x19, x9, #1                          ; alu
	mov	x10, x9                              ; alu
	cas	x10, x19, [x8]                       ; load
	cmp	x10, x9                              ; test
	mov	x9, x10                              ; alu
	b.ne	LBB94_1842                          ; branch
## hot LBB94_1881 (blocks [2054])
LBB94_1881:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1881                          ; branch
## hot LBB94_1900 (blocks [2074])
LBB94_1900:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1900                          ; branch
## hot LBB94_1911 (blocks [2087])
LBB94_1911:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1911                          ; branch
## hot LBB94_1941 (blocks [2119])
LBB94_1941:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1941                          ; branch
## hot LBB94_1960 (blocks [2138])
LBB94_1960:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1960                          ; branch
## hot LBB94_1978 (blocks [2158])
LBB94_1978:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1978                          ; branch
## hot LBB94_1994 (blocks [2175])
LBB94_1994:
	rbit	w10, w8                             ; alu
	clz	w10, w10                             ; alu
	ldrb	w10, [x9, w10, uxtw]                ; gather
	str	x27, [x21, x10]                      ; store
	sub	w10, w8, #1                          ; alu
	and	w8, w10, w8                          ; alu
	tst	w8, #0xff                            ; test
	b.ne	LBB94_1994                          ; branch
## window LBB94_50 (blocks [62, 63, 65, 66, 67])
LBB94_50:
	ldr	w8, [x19, #128]                      ; load
	ldr	x9, [x19, #80]                       ; load
	cmp	x9, #0                               ; test
	csel	x9, x23, x21, eq                    ; alu
	ldr	w9, [x19, x9]                        ; gather
	cmp	w8, w9                               ; test
	b.lo	LBB94_55                            ; branch
	ldr	x8, [sp, #88]                        ; STACK load
	ldr	x0, [x8, #8]                         ; load
	ldr	x8, [x0, #40]                        ; load
	blr	x8                                   ; CALL indirect
	tbz	x0, #63, LBB94_52                    ; branch
LBB94_52:
	cbz	x0, LBB94_1821                       ; branch
	ldr	x8, [sp, #88]                        ; STACK load
	ldr	x8, [x8, #24]                        ; load
	cbz	x8, LBB94_50                         ; branch
	ldaprb	w8, [x8]                          ; load
	cbz	w8, LBB94_50                         ; branch
## window LBB94_297 (blocks [342, 343])
LBB94_297:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_297                    ; branch
## window LBB94_445 (blocks [500, 501])
LBB94_445:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_445                    ; branch
## window LBB94_593 (blocks [656, 657])
LBB94_593:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_593                    ; branch
## window LBB94_741 (blocks [812, 813])
LBB94_741:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_741                    ; branch
## window LBB94_889 (blocks [968, 969])
LBB94_889:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1864                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_889                    ; branch
## window LBB94_1037 (blocks [1124, 1125])
LBB94_1037:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_1037                   ; branch
## window LBB94_1185 (blocks [1280, 1281])
LBB94_1185:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1860                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_1185                   ; branch
## window LBB94_1333 (blocks [1436, 1437])
LBB94_1333:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1864                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_1333                   ; branch
## window LBB94_1481 (blocks [1592, 1593])
LBB94_1481:
	ldr	x8, [x21]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x21                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x20                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_1864                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x19                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_1481                   ; branch
## window LBB94_1945 (blocks [2123, 2124])
LBB94_1945:
	ldr	x8, [x22]                            ; load
	ldr	x8, [x8, #8]                         ; load
	mov	x0, x22                              ; alu
	mov	w1, #0                               ; alu
	mov	x2, x23                              ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_2008                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
	mov	x0, x20                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_1945                   ; branch
## window LBB94_2006 (blocks [2186, 2187, 2188])
LBB94_2004:
	ldr	x0, [sp, #208]                       ; STACK load
	ldrb	w2, [sp, #272]                      ; STACK load
	ldr	x8, [x0]                             ; load
	ldr	x8, [x8, #8]                         ; load
	mov	w1, #0                               ; alu
	blr	x8                                   ; CALL indirect
	tbnz	x0, #63, LBB94_2008                 ; branch
	str	xzr, [sp, #256]                      ; STACK store
LBB94_2006:
	mov	x0, x28                              ; alu
	bl	__ZN11gravity_abi7breaker2T15drain17h14385e69846e7149E ; CALL
	tbz	w0, #0, LBB94_2004                   ; branch
