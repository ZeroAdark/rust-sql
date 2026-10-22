"""Rust legacy-mangling demangler (the toolchain's default `_ZN..E` symbols), stdlib only: the boxes have no c++filt / rustfilt.
`demangle(sym)` -> 'path::to::fn' without the trailing hash; a symbol it cannot read comes back unchanged."""
import re

_ESC = {"$SP$": "@", "$BP$": "*", "$RF$": "&", "$LT$": "<", "$GT$": ">", "$LP$": "(", "$RP$": ")", "$C$": ","}
_U = re.compile(r"\$u([0-9a-f]{2,6})\$")
_HASH = re.compile(r"^h[0-9a-f]{16}$")


def _unescape(s):
    if s.startswith("_$"):
        s = s[1:]
    for k, v in _ESC.items():
        s = s.replace(k, v)
    s = _U.sub(lambda m: chr(int(m.group(1), 16)), s)
    return s.replace("..", "::")


def demangle(sym):
    s = sym.strip('"')
    if s.startswith("__ZN"):
        s = s[1:]
    if not s.startswith("_ZN"):
        return sym
    i, parts = 3, []
    while i < len(s) and s[i] != "E":
        j = i
        while j < len(s) and s[j].isdigit():
            j += 1
        if j == i:
            return sym
        n = int(s[i:j])
        parts.append(s[j:j + n])
        i = j + n
    if parts and _HASH.match(parts[-1]):
        parts = parts[:-1]
    return "::".join(_unescape(p) for p in parts)


def selftest():
    assert demangle("_ZN5alloc7raw_vec11finish_grow17h0123456789abcdefE") == "alloc::raw_vec::finish_grow"
    assert demangle("_ZN67_$LT$alloc..vec..Vec$LT$T$C$A$GT$$u20$as$u20$core..clone..Clone$GT$5clone17h0123456789abcdefE") \
        == "<alloc::vec::Vec<T,A> as core::clone::Clone>::clone"
    assert demangle("memcpy") == "memcpy"
    print("demangle selftest ok")


if __name__ == "__main__":
    selftest()
