"""Check docs/TOOLS.md against the source it claims to be extracted from."""
import io, os, re, glob, sys

# Repo root is the parent of bench/. Deriving it beats hardcoding: this script
# already outlived one move of the checkout, and a stale absolute path fails in
# a way that looks like a missing file rather than a wrong assumption.
os.chdir(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

doc = io.open("docs/TOOLS.md", encoding="utf-8").read()
mod = io.open("src/tools/mod.rs", encoding="utf-8").read()
src = "".join(io.open(f, encoding="utf-8").read() for f in sorted(glob.glob("src/tools/*.rs")))

pairs = []
for m in re.finditer(r'name\s*=\s*"([a-z_]+)"', mod):
    tail = mod[m.end(): m.end() + 1600]
    t = re.search(r"Parameters<([A-Za-z:_]+)>", tail)
    if t:
        pairs.append((m.group(1), t.group(1)))

fails = []

# 1. every registered tool has its own section
print("%-20s %-10s %s" % ("tool", "section", "params documented"))
print("-" * 62)
for name, ty in pairs:
    has_section = ("### `%s`" % name) in doc
    short = ty.split("::")[-1]
    # An empty struct is written `pub struct X {}` on one line. Without this
    # case the non-greedy search runs past it to the next `\n}` and matches an
    # unrelated struct further down the file.
    if re.search(r"pub struct " + short + r" \{\s*\}", src):
        body = ""
    else:
        m = re.search(r"pub struct " + short + r" \{(.*?)\n\}", src, re.S)
        body = m.group(1) if m else ""
    params = re.findall(r"pub ([a-z_]+):", body)
    # serde rename: search_type is exposed as "type"
    renames = dict(re.findall(r'#\[serde\(rename\s*=\s*"([a-z_]+)"\)\]\s*\n\s*pub ([a-z_]+):', src))
    exposed = [k for k, v in renames.items()]
    section = doc.split("### `%s`" % name)[1].split("\n### ")[0] if has_section else ""
    missing = []
    for p in params:
        shown = p
        for jsonname, rustname in renames.items():
            if rustname == p:
                shown = jsonname
        if ("`%s`" % shown) not in section:
            missing.append(shown)
    ok = has_section and not missing
    if not ok:
        fails.append((name, "no section" if not has_section else "missing %s" % missing))
    print("%-20s %-10s %s" % (name, "OK" if has_section else "MISSING",
                              "all" if not missing else "MISSING: %s" % missing))

# 2. profile counts stated in the doc match config.rs
cfg = io.open("src/config.rs", encoding="utf-8").read()
for prof, const in (("core", "PROFILE_CORE"), ("read", "PROFILE_READ"), ("minimal", "PROFILE_MINIMAL")):
    m = re.search(r"const " + const + r": &\[&str\] = &\[(.*?)\];", cfg, re.S)
    real = len(re.findall(r'"[a-z_]+"', m.group(1)))
    stated = re.search(r"\| `" + prof + r"` \| (\d+) \|", doc)
    stated = int(stated.group(1)) if stated else -1
    ok = real == stated
    if not ok:
        fails.append((prof + " profile", "doc says %d, config has %d" % (stated, real)))
    print("profile %-12s doc=%-3s config=%-3s %s" % (prof, stated, real, "OK" if ok else "MISMATCH"))

full_stated = re.search(r"\| `full` \| (\d+) \|", doc)
print("profile %-12s doc=%-3s registered=%-3s %s" % (
    "full", full_stated.group(1), len(pairs),
    "OK" if int(full_stated.group(1)) == len(pairs) else "MISMATCH"))
if int(full_stated.group(1)) != len(pairs):
    fails.append(("full profile", "doc says %s, %d registered" % (full_stated.group(1), len(pairs))))

print()
print("RESULT:", "TOOLS.md matches the source" if not fails else "MISMATCHES: %s" % fails)
sys.exit(1 if fails else 0)
