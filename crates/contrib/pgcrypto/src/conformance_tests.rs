//! C-parity witnesses for audit batch b033-contrib-pgcrypto-1 (contrib/pgcrypto,
//! oracle = live PostgreSQL 18.6 pgcrypto, pair run 2026-09-05). Each test
//! names the C site whose observable behaviour it locks. The crafted keys
//! derive from the C regress `elg1024` keytbl row (sql/pgp-pubkey-decrypt.sql):
//! DSA main key + Elgamal encryption subkey, unprotected secret material.
//! The SQL twin is scripts/pgcrypto-conformance-e2e.sh.

use super::*;
use crate::pgp::consts;

const CTL_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40cec04d0442c8214a100400\
    87948d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6\
    943c2b69329401fb8266035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f\
    88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a0\
    30446237317469c6546267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c\
    9ca8d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2\
    f627e1f2b65fbb9ed46ddc1d1dad2f8967380e2961";
const CTL_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9ec7c0710442c8214a10040087948d6db24c02c27eb106dc584cdd92ef00758079cd8e\
    d0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb8266035c83fd0fb311466fd9c372f8\
    c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac\
    63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267347beffdea07cbd1fb7720ff\
    92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b0593cc2ae54942911f\
    a1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad2f8967380e29610000\
    f74df4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1121";
const MSG1: &str = "85010e03d936cf64bb73f466100400815596d0378bad9fb5b615891813e9d964452fd1de36aa961db28909725bcf5532\
    7da518ef2c55bc6e44bad9a8f893226379a0db7fc38155571d066c4d6a46bc633a7638143462948d90db5ba36883b32b\
    7bbf08cb06adb423a68143fdbc8c308373958ee0c237f9d58ab1901675e924173c0f35aa0cc591583cb533466c2bb003\
    ff6253a2c485c5476938dd032ad218dc62f39a1e8d937895d20f15809294053d6b8425ee3038f7b279bffc4326ee14d8\
    3e59ce6eae26221833f0daa9ae6868a319d4d0a6a9a1f6f73f7c021cfa54d78cf185763c88a3bfdc9841ab6b85731e17\
    b81330916e0074dc58fe6cc96442ed7932fc4ebd2cecf21293ea306f8511a46de7d23b0173409550bc6b8e1e448e4770\
    c02335f7927af8a6d0c4e3854346f7b8e56fbf874481dfe14444423aae443da1619477a0c9827b60ef38daf9d20c";
const K1_LEADZERO_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40cec04d0442c8214a100400\
    87948d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6\
    943c2b69329401fb8266035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f\
    88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8a020c51f1834630008060400042cd877b5679fa9e0e4a0\
    30446237317469c6546267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c\
    9ca8d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2\
    f627e1f2b65fbb9ed46ddc1d1dad2f8967380e2961";
const K2_UNKNOWN_ALGO_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40ce060442c8214a63cec04d\
    0442c8214a10040087948d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea\
    06d19503748a8ca6943c2b69329401fb8266035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb\
    07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd8\
    77b5679fa9e0e4a030446237317469c6546267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4\
    aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d8\
    98ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad2f8967380e2961";
const K3_V3_TRUNC_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40cec04d0442c8214a100400\
    87948d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6\
    943c2b69329401fb8266035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f\
    88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a0\
    30446237317469c6546267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c\
    9ca8d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2\
    f627e1f2b65fbb9ed46ddc1d1dad2f8967380e2961ce020300";
const K4_TRAILING_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9ec7c0740442c8214a10040087948d6db24c02c27eb106dc584cdd92ef00758079cd8e\
    d0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb8266035c83fd0fb311466fd9c372f8\
    c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac\
    63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267347beffdea07cbd1fb7720ff\
    92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b0593cc2ae54942911f\
    a1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad2f8967380e29610000\
    f74df4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1121010203";
const K5_BADCKSUM_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9ec7c0710442c8214a10040087948d6db24c02c27eb106dc584cdd92ef00758079cd8e\
    d0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb8266035c83fd0fb311466fd9c372f8\
    c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac\
    63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267347beffdea07cbd1fb7720ff\
    92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b0593cc2ae54942911f\
    a1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad2f8967380e29610000\
    f74df4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1120";
const K6_DSA_SIGNSUB_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9ec7c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464f\
    d613916c84dd5cad7bae47f1653db463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae\
    8fdd4b63ecfef0a140f66a1dac44c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614a\
    cfa6a9ff5e1313a38b00a09edb61925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6b\
    ad5b4fa5ef0e4a0f234023dc6139626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626\
    e9204dce97fd67104737d0b5489c3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2\
    f327e1bbeff9205bb25be260b88b8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdf\
    f2d9959b8b34f0f672fde82f736fb1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd31\
    8d4293ada03c129af35fd6c22eef22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9ee\
    f79b4000009e3e22e2cb7eb3e0ba58a4c80dc2f5232d126aaf250a9ec7c0710442c8214a10040087948d6db24c02c27e\
    b106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb82\
    66035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae\
    6e4b6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c654\
    6267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca8685\
    70d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed4\
    6ddc1d1dad2f8967380e29610000f74df4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1121";
const K6B_RSA_SIGNSUB_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9ec7c2d60442c825b1030800c021db5ca0b02641f97ca5bef7a4f46e118c7ca96f6494\
    b7c2186e40e0e50e8b4d091cdc0f65a0017b53c6ce3a1e5eb80cd4c4d0661a19bb555d83e28323a5c0b62cbd3fa6cb83\
    b9edb1792c0f8c01a082ad40a736e79a5dba4c538aefb1789246daaf1964f3a9f7b095f5f6d0f6f3c2c209ee4ac0c2a1\
    09626d98a40d53c0c954efa7b271e6bcfcc97b25e2cc6f69e3193eaa853ed03f10d790ddca6cc91d414174afa19a47d4\
    5ab46fc19c65311d5960ffc3afca302442f139a011dfb3ddef2422c0b0150f934876c2731456760644ee0137d6c2471d\
    592b6655ed2d4f0c3d75aab25573474717fb20bc8fa6d723afc1a99d92bd19dd270bae9d102be2e9bb0006290007ff41\
    9b2baf3d1cedd816c06a8c229c3a7cffbf94b6bbe3e7dad821515ad628c0488ab1f0e0ff037b51ad3bd37e9d3613daf1\
    a64fb14e924739b93f26ca39445e0fda4db741712645f48f9a4259f8d22604db735ac276a67fa32064fd4ea942d7ffff\
    37f0dcf6c397b23c4c2e863addb0e4d81c5ddb161cfecb0263dcb4e554cb316c7a77af4743054c790c6e6288d47a459b\
    c5cdfded4597ea88003f8e6a04c5e41df656fa6826d921e67c6722e4e8349b1af48cbb17d30cd827853b24cc8a131c8d\
    7281054680628175d798ab06faaeecdfea5a6cea2e0f32c41679ea687a99e085dfeebb540db2edf2fcae1c3642eaa02e\
    0d4916528054bfbd72b092ae7fff190400d644fb172b56e8134e7df6e5158664ac97abde2728443a9afe58e01ffc9569\
    6a154f58a0cb413d5f360eeccc0fdaeb23feb7ac2f2d0d0b5a82b61939d5c67fc2d868a0c97cf1ee566f8979158c0111\
    a73b9ae4814e8e16f1c007d611634d62fa5c01af133197dccb7da67f2e6069502d9021268342014d843826cdd4f7210e\
    fb0400e58d2987acb94564f3eb104add0d0cd85df415ad4d64c790265bd5d282dc94dfafd3ea91304bad615b62390eb6\
    f74f016ec50da5d75d97dd1f58c759fc0fdd0f88eacaa049a5a9092e1d0409ecdff455b4eedccd583223099ad38888bc\
    9a9d7ab62b7ef914abde4a7f03f657cc17921acf3cfc72ae0e72df0425a58da36f94410400b5ed22b59acf8306d724e2\
    fa5d8e9dd2277d926f20bf0d4df4f3d435da3ef84bb65dbb5a9994a9efcb79163d85ff84cb92cc667e98b54934512a1f\
    65f4d5a173a69633d21748f4f181f6d4c512a65640bf599177e373bf42827b75da936e9116edc3437f932bad39d2051d\
    54fbf7054077ea68c2ace25c9b0229cfb8c63c120a4662c7c0710442c8214a10040087948d6db24c02c27eb106dc584c\
    dd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb8266035c83fd\
    0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c\
    48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267347bef\
    fdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b\
    0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad\
    2f8967380e29610000f74df4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1121";
const K7_LENTYPE3_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40bb0442c8214a1004008794\
    8d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c\
    2b69329401fb8266035c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed\
    30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a03044\
    6237317469c6546267347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8\
    d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627\
    e1f2b65fbb9ed46ddc1d1dad2f8967380e2961";
const K7_LENTYPE3_SEC: &str = "c5c0fb0442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b4000009e3e22e2cb7eb3e0ba\
    58a4c80dc2f5232d126aaf250a9e9f0442c8214a10040087948d6db24c02c27eb106dc584cdd92ef00758079cd8ed0c9\
    ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb8266035c83fd0fb311466fd9c372f8c857\
    ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b6a6d8c48fa1df573744366ac63f8\
    a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267347beffdea07cbd1fb7720ff9248\
    5a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3336f8b0593cc2ae54942911fa1ef\
    6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc1d1dad2f8967380e29610000f74d\
    f4138bb664a39f241a5433e10c9bd566b068da419578b5e07447fab75bcb1121";
const K8_TWO_MAIN_PUB: &str = "c6c0e20442c82148110400a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653d\
    b463c15fb38cb9a5f978667ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44\
    c6928352de3b07b0c675126b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61\
    925471218c37e76657f4ebfda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139\
    626087265280fc8844afc40938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c\
    3c74267b9a005a81dae9b55064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b\
    8d927503ff495c5d5eb0dabe5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736f\
    b1a12071349997ef1798ef292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef\
    22c10140b12dc81986988eb8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40c6c0e20442c82148110400\
    a9e34d4bea35eb2c1dbc737600e2c3373892a7c4cd464fd613916c84dd5cad7bae47f1653db463c15fb38cb9a5f97866\
    7ce0b1541af68fd24eda9df9977039023b73fd1a02feae8fdd4b63ecfef0a140f66a1dac44c6928352de3b07b0c67512\
    6b0bc0d19c8478c1b4b2400ea66b74b5c85a82e692614acfa6a9ff5e1313a38b00a09edb61925471218c37e76657f4eb\
    fda21b7fa7f103ff55e2e0f4beb645d5bf2230e5a65f6bad5b4fa5ef0e4a0f234023dc6139626087265280fc8844afc4\
    0938b57efb9544bf897a290d7f0c5a09ce835daf500626e9204dce97fd67104737d0b5489c3c74267b9a005a81dae9b5\
    5064ec2f6c987798e5c3abf4e4fca176da0a3c99a338e2f327e1bbeff9205bb25be260b88b8d927503ff495c5d5eb0da\
    be5fd1de849caa1c753b2c411ffdc93acc2ee8d9eaccdff2d9959b8b34f0f672fde82f736fb1a12071349997ef1798ef\
    292e6ce32886fe862ef4a95c77b7622132a6dbe01fdd318d4293ada03c129af35fd6c22eef22c10140b12dc81986988e\
    b8629ce844d15e69a17d258fac00aa913aa7ba74b8c9eef79b40cec04d0442c8214a10040087948d6db24c02c27eb106\
    dc584cdd92ef00758079cd8ed0c9ff443af9f4837f5bcc0361c6699bea06d19503748a8ca6943c2b69329401fb826603\
    5c83fd0fb311466fd9c372f8c857ca9d5699ead4fbb3305b816e6188eb07a0ecf8f4d5d94f88ed30b7594b5fe4ae6e4b\
    6a6d8c48fa1df573744366ac63f8a020c51f18346300030603fb042cd877b5679fa9e0e4a030446237317469c6546267\
    347beffdea07cbd1fb7720ff92485a3a84406b90a56164aa0f49246be4aa4d684a1b85dc6c9ca8d46b0feaca868570d3\
    336f8b0593cc2ae54942911fa1ef6bba86ff05ba578257c489513d54d898ed957251d8f7b2f627e1f2b65fbb9ed46ddc\
    1d1dad2f8967380e2961";

fn unhex(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    (0..b.len() / 2)
        .map(|i| u8::from_str_radix(core::str::from_utf8(&b[2 * i..2 * i + 2]).unwrap(), 16).unwrap())
        .collect()
}

fn ctl_pub() -> Vec<u8> {
    unhex(CTL_PUB)
}
fn ctl_sec() -> Vec<u8> {
    unhex(CTL_SEC)
}
fn msg1() -> Vec<u8> {
    unhex(MSG1)
}

fn hash_msg(r: Result<Vec<u8>, hashing::HashError>) -> String {
    match r {
        Ok(v) => hex(&v),
        Err(e) => hash_err(e).message,
    }
}

fn hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}

fn cipher_msg(op: &str, r: Result<Vec<u8>, cipher::CipherError>) -> String {
    match r {
        Ok(_) => "OK".to_string(),
        Err(e) => cipher_err(op, e).message,
    }
}

// px.c:273 parse_cipher_name + pgcrypto.c:511 find_provider: option errors
// are distinct from "No such cipher algorithm"; empty option segments skip.
#[test]
fn cipher_spec_option_errors_are_c_exact() {
    let d = b"0123456789abcdef";
    let k = b"key";
    assert_eq!(
        cipher_msg("encrypt", cipher::encrypt("aes/badopt:1", k, &[], d)),
        "Cannot use \"aes/badopt:1\": Unknown option"
    );
    assert_eq!(
        cipher_msg("encrypt", cipher::encrypt("nope/x:1", k, &[], d)),
        "Cannot use \"nope/x:1\": Unknown option"
    );
    assert_eq!(
        cipher_msg("encrypt", cipher::encrypt("aes/pad", k, &[], d)),
        "Cannot use \"aes/pad\": Badly formatted type"
    );
    assert_eq!(
        cipher_msg("encrypt", cipher::encrypt("aes/pad:bogus", k, &[], d)),
        "Cannot use \"aes/pad:bogus\": No such cipher algorithm"
    );
    assert_eq!(cipher_msg("encrypt", cipher::encrypt("aes//pad:pkcs/", k, &[], d)), "OK");
    assert_eq!(
        cipher::encrypt("aes//pad:pkcs/", k, &[], d).ok(),
        cipher::encrypt("aes", k, &[], d).ok()
    );
}

// pgcrypto.c:288 pg_encrypt (and pg_decrypt / _iv): cipher failures are
// ERRCODE_EXTERNAL_ROUTINE_INVOCATION_EXCEPTION; only find_provider's
// "Cannot use" is ERRCODE_INVALID_PARAMETER_VALUE (pgcrypto.c:513).
#[test]
fn cipher_failures_are_39000_and_provider_lookup_22023() {
    assert_eq!(
        cipher_err("decrypt", cipher::CipherError::DecryptFailed).sqlstate,
        ERRCODE_EXTERNAL_ROUTINE_INVOCATION_EXCEPTION
    );
    assert_eq!(
        cipher_err("encrypt", cipher::CipherError::EncryptFailed).sqlstate,
        ERRCODE_EXTERNAL_ROUTINE_INVOCATION_EXCEPTION
    );
    let e = cipher::encrypt("nope-cbc", b"k", &[], b"x").err().unwrap();
    assert_eq!(cipher_err("encrypt", e).sqlstate, ERRCODE_INVALID_PARAMETER_VALUE);
}

// px.c:204 combo_init: `if (klen > ks) klen = ks` — oversize keys are cut to
// the cipher's max key size (openssl.c cipher table), never refused.
#[test]
fn oversize_keys_clamp_to_cipher_key_size() {
    let d = b"0123456789abcdef";
    let enc = |spec: &str, key: &[u8]| cipher::encrypt(spec, key, &[], d).ok().expect(spec);
    assert_eq!(enc("aes", &[b'a'; 40]), enc("aes", &[b'a'; 32]));
    assert_eq!(enc("aes", &[b'a'; 33]), enc("aes", &[b'a'; 32]));
    assert_eq!(enc("bf", &[b'b'; 60]), enc("bf", &[b'b'; 56]));
    assert_eq!(enc("cast5", &[b'c'; 20]), enc("cast5", &[b'c'; 16]));
    assert_eq!(enc("3des", &[b'd'; 30]), enc("3des", &[b'd'; 24]));
    let ct = enc("aes", &[b'a'; 40]);
    assert_eq!(cipher::decrypt("aes", &[b'a'; 40], &[], &ct).ok().unwrap(), d);
}

// pgcrypto.c:504 find_provider: downcase_truncate_identifier before lookup
// AND before the "Cannot use" message (NAMEDATALEN-1 = 63 bytes).
#[test]
fn provider_names_are_downcased_and_truncated() {
    let x70 = "x".repeat(70);
    assert_eq!(
        hash_msg(hashing::digest(&x70, b"abc")),
        format!("Cannot use \"{}\": No such hash algorithm", "x".repeat(63))
    );
    assert_eq!(
        hash_msg(hashing::digest("SHA9", b"abc")),
        "Cannot use \"sha9\": No such hash algorithm"
    );
    assert_eq!(hash_msg(hashing::digest("SHA256", b"abc")), hash_msg(hashing::digest("sha256", b"abc")));
    assert_eq!(
        hash_msg(hashing::hmac(&"Y".repeat(64), b"k", b"abc")),
        format!("Cannot use \"{}\": No such hash algorithm", "y".repeat(63))
    );
    assert_eq!(
        cipher_msg("encrypt", cipher::encrypt(&"z".repeat(64), b"k", &[], b"x")),
        format!("Cannot use \"{}\": No such cipher algorithm", "z".repeat(63))
    );
}

// pgp.c:364 pgp_set_symkey: an empty key is PXE_ARGUMENT_ERROR on both the
// encrypt and the decrypt wrapper (pgp-pgsql.c:440 / :505).
#[test]
fn empty_symmetric_key_is_illegal_argument() {
    assert_eq!(
        pgp::sym_encrypt(b"Secret.", b"", None, true).unwrap_err(),
        "Illegal argument to function"
    );
    let ct = pgp::sym_encrypt(b"Secret.", b"k", None, true).unwrap();
    assert_eq!(
        pgp::sym_decrypt(&ct, b"", None, true).err().unwrap().message,
        "Illegal argument to function"
    );
}

// pgp-decrypt.c:647 parse_symenc_sesskey: ctx->s2k_count is decoded from
// s2k.iter for EVERY S2K mode (iter is 0 => 1024 for simple/salted).
#[test]
fn s2k_count_is_decoded_for_every_s2k_mode() {
    for mode in [b"s2k-mode=0".as_slice(), b"s2k-mode=1"] {
        let ct = pgp::sym_encrypt(b"Secret.", b"foobar", Some(mode), true).unwrap();
        let out = pgp::sym_decrypt(&ct, b"foobar", Some(b"expect-s2k-count=1024"), true).unwrap();
        assert_eq!(out.notices, Vec::<String>::new(), "{}", String::from_utf8_lossy(mode));
        let out = pgp::sym_decrypt(&ct, b"foobar", Some(b"expect-s2k-count=65536"), true).unwrap();
        assert_eq!(
            out.notices,
            vec!["pgp_decrypt: unexpected s2k_count: expected 65536 got 1024".to_string()]
        );
    }
}

// pgp-decrypt.c:681/612: the encrypted session key must be exactly
// 1 + cipher key size bytes; trailing bytes are PXE_PGP_CORRUPT_DATA.
#[test]
fn encrypted_session_key_length_must_match_cipher() {
    let ct = pgp::sym_encrypt(b"Secret.", b"key", Some(b"sess-key=1"), true).unwrap();
    assert_eq!(ct[0], 0xC3);
    let len = ct[1] as usize;
    let mut bad = vec![0xC3, (len + 3) as u8];
    bad.extend_from_slice(&ct[2..2 + len]);
    bad.extend_from_slice(&[1, 2, 3]);
    bad.extend_from_slice(&ct[2 + len..]);
    assert_eq!(pgp::sym_decrypt(&ct, b"key", None, true).unwrap().plaintext, b"Secret.");
    assert_eq!(
        pgp::sym_decrypt(&bad, b"key", None, true).err().unwrap().message,
        "Wrong key or corrupt data"
    );
}

// pgp-pubkey.c:84 calc_key_id hashes the MPI header bit count as read from
// the packet (pgp_mpi_hash), not a recomputed one: leading zero bits change
// the key id.
#[test]
fn key_id_hashes_mpi_header_bits() {
    assert_eq!(pgp::key_id(&ctl_pub()), Ok("D936CF64BB73F466".to_string()));
    assert_eq!(pgp::key_id(&unhex(K1_LEADZERO_PUB)), Ok("CE9583A4E37FD157".to_string()));
}

// pgp-info.c:77 read_pubenc_keyid: a PKESK whose version byte is not 3
// returns -1, the unused PXE slot, so px_strerror falls through to its
// "Bad error code" text (C oracle: ERROR 39000 Bad error code).
#[test]
fn key_id_pubenc_non_v3_version_is_bad_error_code() {
    assert_eq!(pgp::key_id(&unhex("c109040000000000000000")).unwrap_err(), "Bad error code");
    assert_eq!(pgp::key_id(&unhex("c109030102030405060708")), Ok("0102030405060708".to_string()));
}

// pgp-info.c:156 pgp_get_keyid: a subkey that _pgp_read_public_key rejects
// aborts with that error; pgp-info.c:151/205: several keys is
// PXE_PGP_MULTIPLE_KEYS.
#[test]
fn key_id_propagates_subkey_errors() {
    assert_eq!(
        pgp::key_id(&unhex(K2_UNKNOWN_ALGO_PUB)).unwrap_err(),
        "Unknown public-key encryption algorithm"
    );
    assert_eq!(
        pgp::key_id(&unhex(K3_V3_TRUNC_PUB)).unwrap_err(),
        "Only V4 key packets are supported"
    );
    assert_eq!(pgp::key_id(&unhex(K7_LENTYPE3_PUB)).unwrap_err(), "Wrong key or corrupt data");
    assert_eq!(pgp::key_id(&unhex(K7_LENTYPE3_SEC)).unwrap_err(), "Wrong key or corrupt data");
    assert_eq!(
        pgp::key_id(&unhex(K8_TWO_MAIN_PUB)).unwrap_err(),
        "Several keys given - pgcrypto does not handle keyring"
    );
}

// pgp-pubkey.c:170 _pgp_read_public_key: the version byte is checked before
// anything else is read, so a 2-byte V3 subkey is PXE_PGP_NOT_V4_KEYPKT.
#[test]
fn v4_check_precedes_packet_length_check() {
    assert_eq!(
        pgp::pub_encrypt(b"msg", &unhex(K3_V3_TRUNC_PUB), None, true).unwrap_err().message,
        "Only V4 key packets are supported"
    );
    assert_eq!(
        pgp::pub_encrypt(b"msg", &unhex(K2_UNKNOWN_ALGO_PUB), None, true).unwrap_err().message,
        "Unknown public-key encryption algorithm"
    );
}

// pgp-pubkey.c:483 internal_read_key / pgp-info.c:133: allow_ctx = 0 — an
// old-format indeterminate-length packet header is PXE_PGP_CORRUPT_DATA in
// key material.
#[test]
fn indeterminate_length_packets_are_rejected_in_keys() {
    assert!(pgp::pub_encrypt(b"msg", &ctl_pub(), None, true).is_ok());
    assert_eq!(
        pgp::pub_encrypt(b"msg", &unhex(K7_LENTYPE3_PUB), None, true).unwrap_err().message,
        "Wrong key or corrupt data"
    );
    assert_eq!(
        pgp::pub_decrypt(&msg1(), &unhex(K7_LENTYPE3_SEC), None, None, true).err().unwrap().message,
        "Wrong key or corrupt data"
    );
}

// pgp-pubkey.c:449 process_secret_key: pgp_expect_packet_end after the
// checksum — trailing bytes in a secret subkey packet are corrupt data.
#[test]
fn secret_key_trailing_bytes_are_rejected() {
    assert_eq!(
        pgp::pub_decrypt(&msg1(), &ctl_sec(), None, None, true).unwrap().plaintext,
        b"Secret msg"
    );
    assert_eq!(
        pgp::pub_decrypt(&msg1(), &unhex(K4_TRAILING_SEC), None, None, true).err().unwrap().message,
        "Wrong key or corrupt data"
    );
}

// pgp-pubkey.c:331 check_key_cksum (and :290 check_key_sha1): a mismatch is
// PXE_PGP_KEYPKT_CORRUPT ("Corrupt key packet"), not the generic corrupt-data.
#[test]
fn secret_key_checksum_mismatch_is_corrupt_key_packet() {
    assert_eq!(
        pgp::pub_decrypt(&msg1(), &unhex(K5_BADCKSUM_SEC), None, None, true).err().unwrap().message,
        "Corrupt key packet"
    );
}

// pgp-pubkey.c:412 process_secret_key reads the FULL secret material of
// signing subkeys (RSA sign: d,p,q,u; DSA: x) and validates it; the key is
// then skipped (can_encrypt = 0) and the encryption subkey found.
#[test]
fn signing_subkeys_are_parsed_in_full_then_skipped() {
    for (name, key) in [("dsa", K6_DSA_SIGNSUB_SEC), ("rsa-sign", K6B_RSA_SIGNSUB_SEC)] {
        let out = pgp::pub_decrypt(&msg1(), &unhex(key), None, None, true)
            .unwrap_or_else(|e| panic!("{name}: {}", e.message));
        assert_eq!(out.plaintext, b"Secret msg", "{name}");
    }
}

// pgp-decrypt.c:185 pktreader_pull: the zero-byte pull of a zero-bit final
// MPI at a partial-chunk boundary runs the length loop, so C consumes the
// terminal length octet of this signing subkey (8 + 2 + 1 chunks, then 0)
// and the key still encrypts.
const SIGNSUB_ZERO_E_PARTIAL: &str = "cee30400000000030008e1ff00e00000";

#[test]
fn zero_bit_mpi_at_a_chunk_boundary_consumes_the_next_length_octet() {
    let body = pgp::packet::Body { data: Vec::new(), chunks: vec![(1, 8), (10, 2), (13, 1), (15, 0)], truncated: false };
    assert_eq!(body.pos_after_read(11, 0), 15);
    assert_eq!(body.pos_after_read(11, 1), 14);
    assert_eq!(body.pos_after_read(5, 0), 6);
    let mut key = ctl_pub();
    key.extend_from_slice(&unhex(SIGNSUB_ZERO_E_PARTIAL));
    let msg = pgp::pub_encrypt(b"x", &key, None, false).unwrap_or_else(|e| panic!("{}", e.message));
    let out = pgp::pub_decrypt(&msg, &ctl_sec(), None, None, false).unwrap_or_else(|e| panic!("{}", e.message));
    assert_eq!(out.plaintext, b"x");
}

struct ForceRandomFailure;
impl ForceRandomFailure {
    fn arm() -> ForceRandomFailure {
        consts::FORCE_RANDOM_FAILURE.with(|f| f.set(true));
        ForceRandomFailure
    }
}
impl Drop for ForceRandomFailure {
    fn drop(&mut self) {
        consts::FORCE_RANDOM_FAILURE.with(|f| f.set(false));
    }
}

// px.c:96 px_THROW_ERROR(PXE_NO_RANDOM): every pg_strong_random failure in
// the pgp paths (pgp-s2k.c:237, pgp-encrypt.c:487/586, pgp-pubenc.c:55) and
// pgcrypto.c:471 pg_random_bytes surfaces as "could not generate a random
// number" with ERRCODE_INTERNAL_ERROR.
#[test]
fn rng_failures_report_pxe_no_random() {
    let _f = ForceRandomFailure::arm();
    let want = "could not generate a random number";
    assert_eq!(pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=1"), true).unwrap_err(), want);
    assert_eq!(pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=3"), true).unwrap_err(), want);
    assert_eq!(pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0, sess-key=1"), true).unwrap_err(), want);
    assert_eq!(pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0"), true).unwrap_err(), want);
    assert_eq!(pgp::pub_encrypt(b"x", &ctl_pub(), None, true).unwrap_err().message, want);
    let e = random_bytes(16).unwrap_err();
    assert_eq!(e.message, want);
    assert_eq!(e.sqlstate, ERRCODE_INTERNAL_ERROR);
    assert_eq!(px_msg(want).sqlstate, ERRCODE_INTERNAL_ERROR);
}

// pgp-s2k.c:247 pgp_s2k_fill: an unknown mode is PXE_PGP_BAD_S2K_MODE.
#[test]
fn s2k_fill_bad_mode_message() {
    assert_eq!(crate::pgp::s2k::S2k::fill(7, 2, -1).err(), Some("Bad S2K mode"));
}

struct ForceFipsMode;
impl ForceFipsMode {
    fn arm() -> ForceFipsMode {
        FORCE_FIPS_MODE.with(|f| f.set(true));
        ForceFipsMode
    }
}
impl Drop for ForceFipsMode {
    fn drop(&mut self) {
        FORCE_FIPS_MODE.with(|f| f.set(false));
    }
}

// openssl.c:888 CheckBuiltinCryptoMode over pgcrypto.builtin_crypto_enabled:
// "off" always errors, "fips" errors iff the linked OpenSSL is in FIPS mode
// (openssl.c:847 CheckFIPSMode — the vendored build never is), "on" passes.
// Both ereports carry C's default errcode (XX000).
#[test]
fn builtin_crypto_mode_off_and_fips_arms() {
    assert!(check_builtin_crypto_mode(Some("on")).is_ok());
    assert!(check_builtin_crypto_mode(None).is_ok());
    let off = check_builtin_crypto_mode(Some("off")).err().unwrap();
    assert_eq!(off.message, "use of built-in crypto functions is disabled");
    assert_eq!(off.sqlstate, ERRCODE_INTERNAL_ERROR);
    assert!(!check_fips_mode(), "vendored OpenSSL has no FIPS provider");
    assert!(check_builtin_crypto_mode(Some("fips")).is_ok());
    let _f = ForceFipsMode::arm();
    assert!(check_fips_mode());
    let e = check_builtin_crypto_mode(Some("fips")).err().unwrap();
    assert_eq!(
        e.message,
        "use of non-FIPS validated crypto not allowed when OpenSSL is in FIPS mode"
    );
    assert_eq!(e.sqlstate, ERRCODE_INTERNAL_ERROR);
    assert!(check_builtin_crypto_mode(Some("on")).is_ok());
}

// openssl.c:168 px_find_digest resolves every EVP_get_digestbyname name, not
// a fixed six (batch-19 regression / pgcrypto#1 / b3#3).
// Values: live C 18.6 pgcrypto, 2026-09-13.
#[test]
fn digest_and_hmac_resolve_every_openssl_name() {
    assert_eq!(
        hash_msg(hashing::digest("sha3-256", b"abc")),
        "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
    );
    assert_eq!(
        hash_msg(hashing::hmac("sha3-256", b"key", b"abc")),
        "09b6dbab8d11795ca7c8d82f1cf91682013c7cb980abbb25473be4ae7f7b5683"
    );
    assert_eq!(
        hash_msg(hashing::digest("sha512-256", b"abc")),
        "53048e2681941ef99b2e29b76b4c7dabe4c2d0c634fc6d46e0e2f13107e7af23"
    );
    assert_eq!(
        hash_msg(hashing::hmac("sha512-256", "k".repeat(200).as_bytes(), "a".repeat(300).as_bytes())),
        "a04af968677a2e05ecb31915525a4e3f6d69af8dbed5960a65fcdef42c054611"
    );
    assert_eq!(
        hash_msg(hashing::digest("ripemd160", b"abc")),
        "8eb208f7e05d987a9b044a8e98c6b087f15a0bfc"
    );
    assert_eq!(hash_msg(hashing::hmac("md5", b"k", b"abc")), "75972c9c6569f2f407752ddb02ac79de");
    // openssl.c:101 digest_result_size: an XOF has no fixed size (OpenSSL 3.4+).
    let e = hash_err(hashing::digest("shake128", b"abc").err().unwrap());
    assert_eq!(e.message, "EVP_MD_CTX_size() failed");
    assert_eq!(e.sqlstate, ERRCODE_INTERNAL_ERROR);
}

// pgp.c:174 pgp_load_digest: RIPEMD160 is a pgp digest (batch-19 b2#3).
#[test]
fn s2k_digest_ripemd160_round_trips() {
    let ct = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-digest-algo=ripemd160"), false).unwrap();
    assert_eq!(ct[5], consts::PGP_DIGEST_RIPEMD160 as u8);
    assert_eq!(pgp::sym_decrypt(&ct, b"k", None, false).unwrap().plaintext, b"x");
}

// pgp.c:83 cipher_list: twofish has a 16-byte block and 32-byte key, so the
// prefix is built and pgp_cfb_create is what rejects it (batch-19 b3#1).
#[test]
fn twofish_is_unsupported_not_a_zero_block() {
    for args in [
        "cipher-algo=twofish,s2k-cipher-algo=aes128",
        "cipher-algo=twofish,s2k-cipher-algo=aes,sess-key=1",
        "cipher-algo=twofish",
    ] {
        assert_eq!(
            pgp::sym_encrypt(b"x", b"k", Some(args.as_bytes()), false).unwrap_err(),
            consts::UNSUPPORTED_CIPHER,
            "{args}"
        );
    }
    // The S2K-only arm encrypts (the 32-byte S2K key drives AES-256 as in C)
    // and is rejected when decrypt selects twofish as the data cipher.
    let ct = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-cipher-algo=twofish"), false).unwrap();
    assert_eq!(ct[3], consts::PGP_SYM_TWOFISH as u8);
    match pgp::sym_decrypt(&ct, b"k", None, false) {
        Ok(_) => panic!("decrypt succeeded"),
        Err(e) => assert_eq!(e.message, consts::UNSUPPORTED_CIPHER),
    }
}

// pgp-encrypt.c:263 pkt_stream_process: streamed packets go out in 16 KiB
// partial-length chunks (0xEE) ending in a normal-length chunk, at the
// literal and the encrypted-data level (batch-19 mbuf#1 / pgp-encrypt#3).
// Lengths: live C 18.6, s2k-mode=0 so only the prefix bytes are random.
#[test]
fn stream_packets_use_partial_lengths() {
    let enc = |n: usize, extra: &str| {
        let args = format!("s2k-mode=0,compress-algo=0{extra}");
        pgp::sym_encrypt(&vec![b'x'; n], b"k", Some(args.as_bytes()), false).unwrap()
    };
    let ct = enc(20000, "");
    assert_eq!(ct[7], 0xEE);
    assert_eq!(ct.len(), 20061);
    assert_eq!(enc(0, "").len(), 57);
    assert_eq!(enc(16378, "").len(), 16437);
    assert_eq!(enc(16384, "").len(), 16443);
    assert_eq!(enc(32768, "").len(), 32829);
    assert_eq!(enc(100000, "").len(), 100071);
    assert_eq!(enc(20000, ",disable-mdc=1").len(), 20038);
    assert_eq!(enc(16384, ",disable-mdc=1").len(), 16420);
    let big = vec![b'y'; 40000];
    let ct = pgp::sym_encrypt(&big, b"k", None, true).unwrap();
    assert_eq!(pgp::sym_decrypt(&ct, b"k", None, true).unwrap().plaintext, big);
}

// pgp-decrypt.c:802 parse_literal_data: CRLF conversion follows the SQL
// caller's text mode, not the packet's own type (batch-19 pgp-decrypt#1).
#[test]
fn bytea_decrypt_of_a_text_packet_keeps_crlf() {
    let ct = pgp::sym_encrypt(b"a\r\nb\r", b"k", None, true).unwrap();
    let out = pgp::sym_decrypt(&ct, b"k", Some(b"convert-crlf=1"), false).unwrap();
    assert_eq!(out.plaintext, b"a\r\nb\r");
    let out = pgp::sym_decrypt(&ct, b"k", Some(b"convert-crlf=1"), true).unwrap();
    assert_eq!(out.plaintext, b"a\nb\r");
}

// A no-MDC, s2k-mode=0 AES-128 message whose encrypted stream carries two
// literal packets; pgp-decrypt.c:883 process_data_packets appends both
// (batch-19 pgp-decrypt#2).
fn two_literal_message() -> Vec<u8> {
    use pgp::packet::write_packet;
    let mut s2k = pgp::s2k::S2k::fill(consts::PGP_S2K_SIMPLE, consts::PGP_DIGEST_SHA1, -1).unwrap();
    s2k.process(consts::PGP_SYM_AES_128, b"k").unwrap();
    let mut msg = Vec::new();
    write_packet(&mut msg, consts::PGP_PKT_SYMENC_SESSKEY, &[4, consts::PGP_SYM_AES_128 as u8, 0, consts::PGP_DIGEST_SHA1 as u8]);
    let mut plain = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 15, 16];
    for d in [b'a', b'b'] {
        write_packet(&mut plain, consts::PGP_PKT_LITERAL_DATA, &[b'b', 0, 0, 0, 0, 0, d]);
    }
    let mut cfb = pgp::cfb::PgpCfb::create(consts::PGP_SYM_AES_128, &s2k.key, true, None, false).unwrap();
    let ct = cfb.encrypt(&plain);
    write_packet(&mut msg, consts::PGP_PKT_SYMENC_DATA, &ct);
    msg
}

#[test]
fn every_literal_packet_is_appended() {
    let msg = two_literal_message();
    println!("two_literal_message: {}", hex(&msg));
    assert_eq!(pgp::sym_decrypt(&msg, b"k", None, false).unwrap().plaintext, b"ab");
}

// pgp-pubenc.c:171 / pgp-mpi-openssl.c:266: a zero-bit modulus is PXE_BUG on
// encrypt (negative padding room) and PXE_PGP_MATH_FAILED on decrypt
// (BN_mod_exp by zero); neither may panic (batch-19 b2#1 / b2#2). Keys are
// bare RSA / Elgamal subkey packets with n (p) as the zero-bit MPI.
#[test]
fn zero_modulus_keys_fail_like_openssl() {
    let rsa_pub = unhex("ce0d04000000000100000011010001");
    let elg_pub = unhex("ce0e0400000000100000000101000101");
    assert_eq!(pgp::pub_encrypt(b"x", &rsa_pub, None, false).unwrap_err().message, consts::PGCRYPTO_BUG);
    assert_eq!(pgp::pub_encrypt(b"x", &elg_pub, None, false).unwrap_err().message, consts::PGCRYPTO_BUG);
    let rsa_sec = unhex("c71c04000000000100000011010001000001010001010001010001010008");
    let rsa_msg = unhex("c10d030000000000000000010008ff");
    let elg_sec = unhex("c7140400000000100000000101000101000001010002");
    let elg_msg = unhex("c110030000000000000000100008ff0008ff");
    for (msg, sec) in [(rsa_msg, rsa_sec), (elg_msg, elg_sec)] {
        match pgp::pub_decrypt(&msg, &sec, None, None, false) {
            Ok(_) => panic!("decrypt succeeded"),
            Err(e) => assert_eq!(e.message, consts::MATH_FAILED),
        }
    }
}

// ===== batch-76 (2026-09-14): pgp_decrypt walks the whole message =====

// A symmetric AES-128 / s2k-mode=0 / passphrase "k" MDC message around
// `inner`, with the MDC packet header spelled `mdc_hdr` (pgp-decrypt.c:1015
// parse_symenc_mdc_data: CFB without resync, SHA1 over prefix..header).
fn sym_mdc_message(inner: &[u8], mdc_hdr: &[u8]) -> Vec<u8> {
    use pgp::packet::write_packet;
    let mut s2k = pgp::s2k::S2k::fill(consts::PGP_S2K_SIMPLE, consts::PGP_DIGEST_SHA1, -1).unwrap();
    s2k.process(consts::PGP_SYM_AES_128, b"k").unwrap();
    let mut msg = Vec::new();
    write_packet(&mut msg, consts::PGP_PKT_SYMENC_SESSKEY, &[4, consts::PGP_SYM_AES_128 as u8, 0, consts::PGP_DIGEST_SHA1 as u8]);
    let mut plain: Vec<u8> = (1..=16).collect();
    plain.extend_from_slice(&[15, 16]);
    plain.extend_from_slice(inner);
    plain.extend_from_slice(mdc_hdr);
    let mut md = consts::Digest::new(consts::PGP_DIGEST_SHA1).unwrap();
    md.update(&plain);
    plain.extend_from_slice(&md.finish());
    let mut cfb = pgp::cfb::PgpCfb::create(consts::PGP_SYM_AES_128, &s2k.key, false, None, false).unwrap();
    let mut body = vec![1u8];
    body.extend_from_slice(&cfb.encrypt(&plain));
    write_packet(&mut msg, consts::PGP_PKT_SYMENC_DATA_MDC, &body);
    msg
}

fn literal_packet(payload: &[u8]) -> Vec<u8> {
    let mut body = vec![b'b', 0, 0, 0, 0, 0];
    body.extend_from_slice(payload);
    let mut pkt = Vec::new();
    pgp::packet::write_packet(&mut pkt, consts::PGP_PKT_LITERAL_DATA, &body);
    pkt
}

fn sym_dbg(msg: &[u8]) -> Result<Vec<u8>, (String, Vec<String>)> {
    pgp::sym_decrypt(msg, b"k", Some(b"debug=1"), false)
        .map(|o| o.plaintext)
        .map_err(|e| (e.message, e.notices))
}

fn corrupt(notices: &[&str]) -> Result<Vec<u8>, (String, Vec<String>)> {
    Err((consts::CORRUPT_DATA.to_string(), notices.iter().map(|s| s.to_string()).collect()))
}

// pgp-decrypt.c:84 parse_new_len / :112 parse_old_len: a chunk over
// MAX_CHUNK (16 MiB) is "weird length" (batch-76 b1#4 / pgp-decrypt#7).
#[test]
fn packet_lengths_are_capped_at_max_chunk() {
    assert_eq!(pgp::key_id(&unhex("c300d2ff01000001")).unwrap_err(), consts::CORRUPT_DATA);
    let mut msg = pgp::sym_encrypt(b"x", b"k", None, true).unwrap();
    msg.extend_from_slice(&unhex("c3ff01000001"));
    assert_eq!(sym_dbg(&msg), corrupt(&["dbg: parse_new_len: weird length"]));
}

// pgp-decrypt.c:201 pktreader_pull returns the source EOF as a short read,
// so pgp_skip_packet (pgp-info.c:179) accepts a truncated body; a stream's
// next length octet at EOF is still a GETBYTE failure (batch-76 b1#5 /
// pgp-decrypt#8 / mbuf#2).
#[test]
fn skip_packet_accepts_truncated_bodies() {
    assert_eq!(pgp::key_id(&unhex("c301")).unwrap(), "SYMKEY");
    assert_eq!(pgp::key_id(&unhex("c3e1")).unwrap(), "SYMKEY");
    assert_eq!(pgp::key_id(&unhex("c3e100")).unwrap(), "SYMKEY");
    assert_eq!(pgp::key_id(&unhex("c3e10000")).unwrap_err(), consts::CORRUPT_DATA);
    assert_eq!(pgp::key_id(&unhex("c40a")).unwrap_err(), consts::CORRUPT_DATA);
}

// pgp-decrypt.c:1108 pgp_decrypt keeps walking after the data packet,
// dispatches session-key packets by tag and diagnoses repeats (batch-76
// pgp-decrypt#3 / #4 / #5 / #6).
#[test]
fn message_walk_follows_pgp_decrypt() {
    let msg = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0"), true).unwrap();
    let cat = |a: &[u8], b: &[u8]| [a, b].concat();
    assert_eq!(sym_dbg(&cat(&msg, &[0])), corrupt(&["dbg: pgp_parse_pkt_hdr: not pkt hdr"]));
    assert_eq!(sym_dbg(&cat(&msg, &[0xca, 0])).unwrap(), b"x");
    assert_eq!(sym_dbg(&cat(&msg, &[0xc3])), corrupt(&["dbg: pullf_read_fixed: need=1 got=0"]));
    assert_eq!(sym_dbg(&cat(&msg, &msg)), corrupt(&["dbg: pgp_decrypt: using first of several keys"]));
    assert_eq!(sym_dbg(&cat(&msg[..6], &msg)), corrupt(&["dbg: pgp_decrypt: using first of several keys"]));
    assert_eq!(sym_dbg(&cat(&[0xc2, 0], &msg)), corrupt(&["dbg: pgp_decrypt: unknown tag: 0x02"]));
    assert_eq!(sym_dbg(&msg[6..]), corrupt(&["dbg: pgp_decrypt: have data but no key"]));
    assert_eq!(sym_dbg(&cat(&msg, &msg[6..])), corrupt(&["dbg: pgp_decrypt: several data pkts not supported"]));
    let nomdc = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0,disable-mdc=1"), true).unwrap();
    assert_eq!(sym_dbg(&cat(&nomdc, &nomdc[6..])), corrupt(&["dbg: pgp_decrypt: got second data packet"]));
    let mut pubtag = msg.clone();
    pubtag[0] = 0xc1;
    assert_eq!(sym_dbg(&pubtag), Err((consts::PGCRYPTO_BUG.to_string(), vec!["dbg: no pubkey?".to_string()])));
    let mut symtag = msg1();
    symtag[0] = 0x8d;
    let err = pgp::pub_decrypt(&symtag, &ctl_sec(), None, Some(b"debug=1"), false).unwrap_err();
    assert_eq!((err.message.as_str(), &err.notices[..]), (consts::CORRUPT_DATA, &["dbg: bad key pkt ver".to_string()][..]));
}

// pgp-decrypt.c:873 process_data_packets under NEED_MDC: the MDC packet is
// parsed by pgp_parse_pkt_hdr (any encoding of length 20), every byte read
// is hashed, and the MDC filter's EOF / a short MDC body carry mdc_read's
// and mdc_finish's px_debug (batch-76 pgp-decrypt#9).
#[test]
fn mdc_packet_is_walked_like_any_packet() {
    assert_eq!(sym_dbg(&sym_mdc_message(&literal_packet(b"x"), &[0xd3, 0x14])).unwrap(), b"x");
    assert_eq!(sym_dbg(&sym_mdc_message(&literal_packet(b"x"), &[0xd3, 0xff, 0, 0, 0, 0x14])).unwrap(), b"x");
    let msg = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0"), true).unwrap();
    assert_eq!(sym_dbg(&msg[..msg.len() - 5]), corrupt(&["dbg: mdc_finish: read failed, res=15"]));
    assert_eq!(sym_dbg(&msg[..msg.len() - 30]), corrupt(&["dbg: mdc_read: unexpected eof"]));
    assert_eq!(sym_dbg(&msg[..9]), corrupt(&["dbg: mdc_read: unexpected eof"]));
}

// pgp-decrypt.c:788 parse_literal_data reads the payload until the packet
// reader's EOF, so without an MDC a truncated payload is returned as is
// while a truncated header is "unexpected eof".
#[test]
fn no_mdc_literal_accepts_a_truncated_payload() {
    let msg = pgp::sym_encrypt(b"hello", b"k", Some(b"s2k-mode=0,disable-mdc=1,compress-algo=0"), true).unwrap();
    assert_eq!(sym_dbg(&msg[..msg.len() - 2]).unwrap(), b"hel");
    assert_eq!(sym_dbg(&msg[..msg.len() - 7]), corrupt(&["dbg: parse_literal_data: unexpected eof"]));
    assert_eq!(sym_dbg(&msg[..26]), corrupt(&["dbg: process_data_packets: no data"]));
}

// pgp-compress.c:298 decompress_read: after Z_STREAM_END one more source
// read must be empty; the decompressor's input arrives in reads the decrypt
// filter cuts at 4096 bytes, so bytes in the read that ended the stream are
// not "extra" (batch-76 b1#3 / bug_36a6958c).
#[test]
fn compressed_stream_extra_bytes_follow_the_read_window() {
    let comp = pgp::compress::deflate_raw(&literal_packet(b"x"), 6);
    let packet = |total: usize| {
        let mut body = vec![consts::PGP_COMPR_ZIP as u8];
        body.extend_from_slice(&comp);
        body.resize(total, 0);
        let mut pkt = Vec::new();
        pgp::packet::write_packet(&mut pkt, consts::PGP_PKT_COMPRESSED_DATA, &body);
        sym_mdc_message(&pkt, &[0xd3, 0x14])
    };
    assert_eq!(sym_dbg(&packet(1 + comp.len())).unwrap(), b"x");
    assert_eq!(sym_dbg(&packet(1 + comp.len() + 100)).unwrap(), b"x");
    assert_eq!(sym_dbg(&packet(4097)).unwrap(), b"x");
    let extra = corrupt(&["dbg: decompress_read: extra bytes after end of stream"]);
    assert_eq!(sym_dbg(&packet(4098)), extra);
    assert_eq!(sym_dbg(&packet(8193)), extra);
}

// pgp-pubdec.c:243 pgp_expect_packet_end after the session-key MPI(s)
// (batch-76 b2#5 / pgp-decrypt#10 / bug_094e1212).
#[test]
fn pubenc_sesskey_trailing_bytes_are_rejected() {
    let raw = msg1();
    let mut rdr = pgp::packet::PktReader::new(&raw);
    let hdr = rdr.read_hdr().unwrap().unwrap();
    assert_eq!(hdr.tag, consts::PGP_PKT_PUBENC_SESSKEY);
    let body = rdr.read_body_avail(&hdr).unwrap().data;
    let rest = &raw[rdr.pos..];
    let mut extended = body.clone();
    extended.push(0);
    let mut msg = Vec::new();
    pgp::packet::write_packet(&mut msg, consts::PGP_PKT_PUBENC_SESSKEY, &extended);
    msg.extend_from_slice(rest);
    let err = pgp::pub_decrypt(&msg, &ctl_sec(), None, Some(b"debug=1"), false).unwrap_err();
    assert_eq!(err.message, consts::CORRUPT_DATA);
    assert_eq!(err.notices, ["dbg: pgp_expect_packet_end: got data"]);
    assert_eq!(pgp::pub_decrypt(&raw, &ctl_sec(), None, None, false).unwrap().plaintext, b"Secret msg");
}

// pgp-mpi-openssl.c:45 mpi_to_bn: the declared bit count must equal
// BN_num_bits of the value (batch-76 b2#4).
#[test]
fn mpi_declared_bits_must_match_the_value() {
    let mut msg = msg1();
    let bits = ((msg[13] as usize) << 8) | msg[14] as usize;
    let declared = if bits % 8 != 1 { bits - 1 } else { bits + 1 };
    msg[13] = (declared >> 8) as u8;
    msg[14] = declared as u8;
    let err = pgp::pub_decrypt(&msg, &ctl_sec(), None, Some(b"debug=1"), false).unwrap_err();
    assert_eq!(err.message, consts::MATH_FAILED);
    assert_eq!(err.notices, [format!("dbg: mpi_to_bn: bignum conversion failed: mpi={declared}, bn={bits}")]);
}

// pgp-pubkey.c:529 internal_read_key frees the public subkey's reader
// without draining it: a trailing byte is parsed as the next header
// (batch-76 pgp-pubkey#1).
#[test]
fn public_subkey_tail_is_parsed_as_a_header() {
    let key = |tail: &str| unhex(&format!("{}{}{}", "040000000001040080", "00".repeat(126), tail));
    let mut exact = vec![0xce, 0x8b];
    exact.extend_from_slice(&key("01000101"));
    assert!(pgp::pub_encrypt(b"x", &exact, None, true).is_ok());
    let mut trailing = vec![0xce, 0x8c];
    trailing.extend_from_slice(&key("0100010100"));
    assert_eq!(pgp::pub_encrypt(b"x", &trailing, None, true).unwrap_err().message, consts::CORRUPT_DATA);
}

// mbuf.c:79 prepare_room: the decrypt output MBuf (pgp-pgsql.c:479,
// VARSIZE_ANY(data) + 2048) grows by 2*STEP per append, and the growth
// past MaxAllocSize is mcxt.c's error with that request size (batch-76
// mbuf#3). The 1 GB plaintext is stood in for by a capacity near the top.
#[test]
fn decrypt_output_grows_like_prepare_room() {
    let mut ctx = pgp::context::PgpContext::default();
    ctx.mbuf_cap = ::mcx::MAX_ALLOC_SIZE - 10;
    assert_eq!(
        pgp::decrypt::mbuf_room(&mut ctx, ::mcx::MAX_ALLOC_SIZE - 9).unwrap_err(),
        format!("invalid memory alloc request size {}", ::mcx::MAX_ALLOC_SIZE - 10 + 32768)
    );
    ctx.mbuf_cap = 1_071_735;
    assert_eq!(pgp::decrypt::mbuf_room(&mut ctx, 1_073_732_216).unwrap_err(), "invalid memory alloc request size 1073764983");
    let msg = pgp::sym_encrypt(b"hello world", b"k", Some(b"s2k-mode=0,compress-algo=0"), true).unwrap();
    let mut ctx = pgp::context::PgpContext::default();
    ctx.text_mode = 1;
    ctx.mbuf_cap = 5;
    assert_eq!(pgp::decrypt::decrypt_symmetric(&mut ctx, &msg, b"k").unwrap(), b"hello world");
    assert_eq!(ctx.mbuf_cap, 5 + 32768);
}

// pgp-armor.c:222: enlargeStringInfo(pg_base64_enc_len(len)) after the
// header lines; armor(800 MB) on C 18.6 is 54000 "Cannot enlarge string
// buffer containing 29 bytes by 1080701755 more bytes." (batch-76 pgp-armor#2).
#[test]
fn armor_estimate_matches_pg_base64_enc_len() {
    assert_eq!(pgp::armor::base64_enc_len(800_000_000), 1_080_701_755);
    assert_eq!(pgp::armor::armor_header_len(&[], &[]), 29);
    assert_eq!(pgp::armor::armor_header_len(&[b"k".to_vec()], &[b"vv".to_vec()]), 29 + 6);
    assert!(stringinfo::enlarge_target(29, 29, 1_080_701_755).is_err());
    assert!(stringinfo::enlarge_target(29, 29, pgp::armor::base64_enc_len(700_000_000)).is_ok());
}

// pgp-encrypt.c:397 init_litdata_packet stores time(NULL) in the literal
// packet's creation-time field (batch-76 pgp-encrypt#4).
#[test]
fn literal_packet_carries_the_creation_time() {
    let before = ::pg_clock::wall_timeval().0 as u32;
    let msg = pgp::sym_encrypt(b"x", b"k", Some(b"s2k-mode=0,disable-mdc=1,compress-algo=0"), false).unwrap();
    let after = ::pg_clock::wall_timeval().0 as u32;
    let mut s2k = pgp::s2k::S2k::fill(consts::PGP_S2K_SIMPLE, consts::PGP_DIGEST_SHA1, -1).unwrap();
    s2k.process(consts::PGP_SYM_AES_128, b"k").unwrap();
    let mut rdr = pgp::packet::PktReader::new(&msg);
    rdr.read_hdr().unwrap().unwrap();
    rdr.read_body_avail(&pgp::packet::PktHdr { tag: 3, len: Some(4), partial: false }).unwrap();
    let hdr = rdr.read_hdr().unwrap().unwrap();
    assert_eq!(hdr.tag, consts::PGP_PKT_SYMENC_DATA);
    let body = rdr.read_body_avail(&hdr).unwrap().data;
    let mut cfb = pgp::cfb::PgpCfb::create(consts::PGP_SYM_AES_128, &s2k.key, true, None, false).unwrap();
    let plain = cfb.decrypt(&body);
    assert_eq!(&plain[18..22], &[0xcb, 7, b'b', 0]);
    let t = u32::from_be_bytes([plain[22], plain[23], plain[24], plain[25]]);
    assert!(before <= t && t <= after, "{before} <= {t} <= {after}");
}

// pgp.c:180 pgp_load_digest: an S2K digest code outside the table is
// PXE_PGP_CORRUPT_DATA, not PXE_PGP_UNSUPPORTED_HASH — in a symmetric
// session-key packet (pgp-s2k.c:288) and in a protected secret subkey
// (pgp-pubkey.c:369) (batch-239 b2#6).
#[test]
fn unknown_s2k_digest_code_is_corrupt_data() {
    let err = pgp::sym_decrypt(&unhex("c30404070077"), b"k", Some(b"debug=1"), false).unwrap_err();
    assert_eq!((err.message.as_str(), err.notices.len()), (consts::CORRUPT_DATA, 0));
    assert_eq!(pgp::sym_decrypt(&unhex("c30404070008"), b"k", None, false).unwrap_err().message, consts::CORRUPT_DATA);
    let key = unhex("c713040000000010000101000202000101ff070077");
    let err = pgp::pub_decrypt(&[0], &key, Some(b"p"), Some(b"debug=1"), false).unwrap_err();
    assert_eq!((err.message.as_str(), err.notices.len()), (consts::CORRUPT_DATA, 0));
    assert!(consts::Digest::load(consts::PGP_DIGEST_SHA256).is_ok());
}

// pgp-decrypt.c:634 parse_symenc_sesskey / pgp-s2k.c:253 pgp_s2k_read /
// mbuf.c:311 pullf_read_fixed: every short read, the bad separate-key
// length and decrypt_key's length check leave their px_debug line
// (batch-239 mbuf#4, pgp-decrypt#11).
#[test]
fn symenc_sesskey_short_reads_are_debugged() {
    let dbg = |hex: &str| {
        let err = pgp::sym_decrypt(&unhex(hex), b"k", Some(b"debug=1"), false).unwrap_err();
        assert_eq!(err.message, consts::CORRUPT_DATA, "{hex}");
        err.notices
    };
    assert_eq!(dbg("c30104"), ["dbg: pullf_read_fixed: need=1 got=0"]);
    assert_eq!(dbg("c3020407"), ["dbg: pullf_read_fixed: need=1 got=0"]);
    assert_eq!(dbg("c3020507"), ["dbg: bad key pkt ver"]);
    assert_eq!(dbg("c304040700"), ["dbg: pullf_read_fixed: need=1 got=0"]);
    assert_eq!(dbg("c30604070108aabb"), ["dbg: pullf_read_fixed: need=8 got=2"]);
    assert_eq!(dbg("c30c04070308000000000000000000"), ["dbg: pullf_read_fixed: need=1 got=0"]);
    assert_eq!(dbg("c309040700080102030405"), ["dbg: expect key, but bad data"]);
    assert_eq!(dbg(&format!("c31504070008{}", "00".repeat(17))), ["dbg: sesskey bad len: algo=214, expected=0, got=16"]);
    assert_eq!(dbg(&format!("c32604070008{}", "00".repeat(34))), ["dbg: expect key, but bad data"]);
    assert_eq!(dbg(&format!("c32504070008{}", "00".repeat(33))), ["dbg: sesskey bad len: algo=214, expected=0, got=32"]);
}

// pgp-pubdec.c:172 pgp_parse_pubenc_sesskey: the version byte, then the
// 8-byte key id (compared before the algorithm byte is read), each short
// read debugged (batch-239 b2#8).
#[test]
fn pubenc_sesskey_key_id_is_checked_before_the_algorithm() {
    let key = unhex("c715040000000010000101000202000101000001010002");
    let run = |hex: &str| pgp::pub_decrypt(&unhex(hex), &key, Some(b""), Some(b"debug=1"), false).unwrap_err();
    let err = run("c10903ffffffffffffffff");
    assert_eq!((err.message.as_str(), &err.notices[..]), ("Wrong key", &["dbg: key_id's does not match".to_string()][..]));
    let err = run("c10503ffffffff");
    assert_eq!((err.message.as_str(), &err.notices[..]), (consts::CORRUPT_DATA, &["dbg: pullf_read_fixed: need=8 got=4".to_string()][..]));
    let err = run("c10003");
    assert_eq!(err.notices, ["dbg: pullf_read_fixed: need=1 got=0"]);
    let err = run("c10a030000000000000000");
    assert_eq!(err.notices, ["dbg: pullf_read_fixed: need=1 got=0"]);
}

// pgp-pubkey.c px_debug trail of the key readers: check_key_cksum (:331),
// process_secret_key's hide type (:405), _pgp_read_public_key's algorithm
// (:235), internal_read_key's packet tag (:524), pgp_mpi_read short reads
// (pgp-mpi.c:87) and pgp_expect_packet_end (:449) — on pgp_pub_decrypt AND
// pgp_pub_encrypt (batch-239 pgp-pubkey#5).
#[test]
fn key_reader_debug_trail_follows_c() {
    let dec = |hex: &str| pgp::pub_decrypt(&[0], &unhex(hex), Some(b""), Some(b"debug=1"), false).unwrap_err();
    let err = dec("c715040000000010000101000202000101000001010003");
    assert_eq!((err.message.as_str(), &err.notices[..]), ("Corrupt key packet", &["dbg: key cksum check failed".to_string()][..]));
    let err = dec("c71004000000001000010100020200010105");
    assert_eq!((err.message.as_str(), &err.notices[..]), ("Corrupt key packet", &["dbg: unknown hide type".to_string()][..]));
    assert_eq!(dec("c70e04000000001000010100020200010105").notices, ["dbg: pullf_read_fixed: need=1 got=0"]);
    let err = dec("c706040000000099");
    assert_eq!((err.message.as_str(), &err.notices[..]), (consts::UNKNOWN_PUBALGO, &["dbg: unknown public algo: 153".to_string()][..]));
    let err = dec("c400");
    assert_eq!((err.message.as_str(), &err.notices[..]), ("Unexpected packet in key data", &["dbg: unknown/unexpected packet: 4".to_string()][..]));
    assert_eq!(dec("c70d040000000010000101000202000101").notices, ["dbg: pullf_read_fixed: need=2 got=1"]);
    assert_eq!(dec("c7050400000000").notices, ["dbg: pullf_read_fixed: need=1 got=0"]);
    assert_eq!(dec("c70304000000").notices, ["dbg: pullf_read_fixed: need=4 got=2"]);
    assert_eq!(dec("c70a040000000010000101").notices, ["dbg: pullf_read_fixed: need=2 got=0"]);
    let err = dec("c714040000000010000101000202000101fe00000000");
    assert_eq!((err.message.as_str(), err.notices.len()), (consts::UNSUPPORTED_CIPHER, 0));
    assert_eq!(dec("c71104000000001000010100020200010100000101").notices, ["dbg: pullf_read_fixed: need=2 got=1"]);
    let err = dec("c71604000000001000010100020200010100000101000200");
    assert_eq!((err.message.as_str(), &err.notices[..]), (consts::CORRUPT_DATA, &["dbg: pgp_expect_packet_end: got data".to_string()][..]));
    let err = pgp::pub_encrypt(b"x", &unhex("ce06040000000099"), Some(b"debug=1"), true).unwrap_err();
    assert_eq!((err.message.as_str(), &err.notices[..]), (consts::UNKNOWN_PUBALGO, &["dbg: unknown public algo: 153".to_string()][..]));
    assert_eq!(pgp::key_id(&unhex("c71004000000001000010100020200010105")), Ok("772270DC92C673F3".to_string()));
    assert_eq!(pgp::key_id(&unhex("c70e04000000001000010100020200010105")).unwrap_err(), consts::CORRUPT_DATA);
}

// px.c:206 combo_init hands the cipher a zeroed key buffer with klen 0:
// BF_set_key (bf_skey.c:38) cycles key[0] into every P-array word and
// CAST_set_key (c_skey.c:41) zero-fills x[], so an empty key is the
// one-byte zero key, not a failure (batch-239 openssl#4).
#[test]
fn empty_blowfish_and_cast5_keys_are_zero_keys() {
    let d = b"0123456789abcdef";
    for spec in ["bf", "bf-ecb", "cast5", "cast5-ecb"] {
        let ct = cipher::encrypt(spec, b"", &[], d).ok().expect(spec);
        assert_eq!(ct, cipher::encrypt(spec, &[0], &[], d).ok().unwrap(), "{spec}");
        assert_eq!(cipher::decrypt(spec, b"", &[], &ct).ok().unwrap(), d, "{spec}");
    }
}
