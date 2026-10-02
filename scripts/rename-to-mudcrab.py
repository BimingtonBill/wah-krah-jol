#!/usr/bin/env python3
"""Rename OpenSkyrim / Wah Krah Jol to Mudcrab across every tracked text file.

Run from anywhere inside the repository. Re-runnable on a fresh checkout, so the
rename never needs hand-resolved merge conflicts: reset to fresh main, run this,
run `cargo fmt --all`, re-apply the small hand-fix commit.

    python scripts/rename-to-mudcrab.py          # apply, print per-file counts
    python scripts/rename-to-mudcrab.py --check  # exit 1 and list files still to rename
    python scripts/rename-to-mudcrab.py --self-test  # rules on inline samples, no files

Idempotent: a second run changes nothing. Line endings and encoding are kept
(files are read as bytes, only the matched text changes). Binary files (NUL
byte), `vendor/**` and this script are skipped.

EXCEPTIONS, left unchanged on purpose (see PROTECTED):
  * OPEN_SKYRIM_material (and any OPEN_SKYRIM token): the glTF extension name
    written into converted assets; assets converted earlier must keep loading.
  * The glTF extras key "openSkyrim" (JSON key and /openSkyrim/ pointer): the
    same converted-asset data format.
  * openskyrimdev@gmail.com: a real contact address.
  * ko_fi: wahkrahjol: an external account handle.
  * OPENSKYRIM_* env var fallbacks and the "openSkyrimCollision" reader
    fallback: the old names stay accepted next to the new ones.
  * Paths on a contributor's machine: a lowercase `openskyrim` right after
    a slash or backslash (e.g. /home/dev/.cache/openskyrim/...) and the folder name
    `OpenSkyrim-lod`.
  * OPENSKYRIM_* written with a literal asterisk and no name after it (docs
    saying the old names are still accepted). OPENSKYRIM_*_FIXTURE is renamed.

The collision extras key the converter writes becomes mudcrabCollision; the
engine reader accepts both (see PRE_RULES).
"""

import argparse
from pathlib import Path
import re
import subprocess
import sys

SELF = "scripts/rename-to-mudcrab.py"

# Never changed. Masked with placeholders before the rules, restored after, so
# the same patterns also keep the fallbacks that PRE_RULES write on a re-run.
PROTECTED = [
    r"OPEN_SKYRIM\w*",
    r'"openSkyrim"',
    r"/openSkyrim/",
    r"openskyrimdev@gmail\.com",
    r"ko_fi: wahkrahjol",
    r'\|\| (?:std::)?env::var_os\("OPENSKYRIM_\w+"\)',
    r'\|_\| (?:std::)?env::var\("OPENSKYRIM_\w+"\)',
    r"(?<=[/\\])openskyrim",
    r"OpenSkyrim-lod",
    r'replace\("MUDCRAB_", "OPENSKYRIM_"\)',
    r'get\("openSkyrimCollision"\)',
    r"OPENSKYRIM_\*(?![\w*])",
]

# (pattern, replacement, note, only for paths ending with). Applied first, in
# order, on the raw text. Their output is masked by PROTECTED afterwards.
PRE_RULES = [
    (
        # Option-returning read: new name first, old name as a fallback inline
        # (rustfmt reflows the chain). `(?<!\|\| )` skips an existing fallback.
        r'(?<!\|\| )(?<!::)((?:std::)?env::var_os)\("OPENSKYRIM_(\w+)"\)',
        r'\1("MUDCRAB_\2").or_else(|| \1("OPENSKYRIM_\2"))',
        "env var_os read: MUDCRAB_ first, OPENSKYRIM_ still accepted",
        ".rs",
    ),
    (
        # Result-returning read; works inside .ok()/.map chains too.
        # `(?<!\|_\| )` skips an existing fallback.
        r'(?<!\|_\| )(?<!::)((?:std::)?env::var)\("OPENSKYRIM_(\w+)"\)',
        r'\1("MUDCRAB_\2").or_else(|_| \1("OPENSKYRIM_\2"))',
        "env var read: MUDCRAB_ first, OPENSKYRIM_ still accepted",
        ".rs",
    ),
    (
        # Helper that takes the variable name as a parameter (texture.rs).
        r'(fn convert_installed_2d_fixture\(variable: &str, encoding: TextureEncoding\) \{)(\r?\n)'
        r'([ \t]+)let path = std::env::var_os\(variable\)(\r?\n)'
        r'([ \t]+)\.map\(std::path::PathBuf::from\)(\r?\n)'
        r'[ \t]+\.unwrap_or_else\(\|\| panic!\("set \{variable\} to an installed DDS"\)\);',
        r'\1\2\3let legacy_var = variable.replace("MUDCRAB_", "OPENSKYRIM_");\2'
        r'\3let path = std::env::var_os(variable)\4'
        r'\5.or_else(|| std::env::var_os(&legacy_var))\4'
        r'\5.map(std::path::PathBuf::from)\6'
        r'\5.unwrap_or_else(|| panic!("set {variable} or {legacy_var} to an installed DDS"));',
        "helper also accepts the OPENSKYRIM_ name",
        ".rs",
    ),
    (
        # Engine reader: new key first, old converted assets still load.
        r'(?<!\|\| )value\.get\("openSkyrimCollision"\)',
        r'value.get("mudcrabCollision").or_else(|| value.get("openSkyrimCollision"))',
        "collision reader accepts both keys",
        ".rs",
    ),
]

# (pattern, replacement, note). Applied in order on the masked text.
RULES = [
    # .gitignore: unused config file name, dropped (runs before the name rules)
    (r"^openskyrim\.cfg\r?\n", "", "unused .gitignore entry"),
    # 3. collision extras key (writer, docs, snapshot)
    (r"openSkyrimCollision", "mudcrabCollision", "collision extras key"),
    # 4. URLs and repository names
    (r"github\.com/realfakenerd/OpenSkyrim", "github.com/Mudcrab-Team/mudcrab", "repo URL"),
    (r"github\.com/realfakenerd/wah-krah-jol", "github.com/Mudcrab-Team/mudcrab", "repo URL"),
    (r"realfakenerd%2Fwah-krah-jol", "Mudcrab-Team%2Fmudcrab", "encoded repo name in badge URL"),
    (r"realfakenerd/wah-krah-jol", "Mudcrab-Team/mudcrab", "repo link text"),
    (r"<your-username>/wah-krah-jol\.git", "<your-username>/mudcrab.git", "clone URL"),
    (r"cd wah-krah-jol", "cd mudcrab", "clone directory"),
    (r"wah-krah-jol", "mudcrab", "remaining repo or folder name"),
    # 5. names
    (r"Wah Krah Jol", "Mudcrab", "project name"),
    (r"OpenSkyrim", "Mudcrab", "project name"),
    (r"OPENSKYRIM_", "MUDCRAB_", "env var prefix"),
    (r"openskyrim", "mudcrab", "lowercase name (temp dirs, thread, luarocks, types file)"),
]


def repo_root():
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    )
    return Path(out.stdout.strip())


def tracked_files(root):
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, capture_output=True, check=True
    ).stdout
    return [name for name in out.decode("utf-8").split("\0") if name]


def rewrite(path, text):
    """Return (new text, number of replacements)."""
    count = 0
    for pattern, replacement, _note, suffix in PRE_RULES:
        if path.endswith(suffix):
            text, n = re.subn(pattern, replacement, text)
            count += n
    masked = []

    def mask(match):
        masked.append(match.group(0))
        return f"\x00{len(masked) - 1}\x00"

    text = re.sub("|".join(f"(?:{p})" for p in PROTECTED), mask, text)
    for pattern, replacement, _note in RULES:
        flags = re.MULTILINE if pattern.startswith("^") else 0
        text, n = re.subn(pattern, replacement, text, flags=flags)
        count += n
    text = re.sub(r"\x00(\d+)\x00", lambda m: masked[int(m.group(1))], text)
    return text, count


# (name, path, input, expected). Each rule and each exception, plus rerun
# (idempotency) is asserted on the expected text.
SELF_TESTS = [
    ("name", "a.md", "Wah Krah Jol and OpenSkyrim", "Mudcrab and Mudcrab"),
    ("lowercase", "a.rs", 'join("openskyrim-x")', 'join("mudcrab-x")'),
    ("env prefix in text", "a.md", "set OPENSKYRIM_SKYRIM_DATA", "set MUDCRAB_SKYRIM_DATA"),
    (
        "var_os read",
        "a.rs",
        'std::env::var_os("OPENSKYRIM_NIF_FIXTURE")\n    .map(f)',
        'std::env::var_os("MUDCRAB_NIF_FIXTURE").or_else(|| std::env::var_os("OPENSKYRIM_NIF_FIXTURE"))\n    .map(f)',
    ),
    (
        "var read in an or_else chain",
        "a.rs",
        'a.or_else(|| std::env::var("OPENSKYRIM_NIF_DIR").ok())',
        'a.or_else(|| std::env::var("MUDCRAB_NIF_DIR").or_else(|_| std::env::var("OPENSKYRIM_NIF_DIR")).ok())',
    ),
    (
        "var read without std::",
        "a.rs",
        'env::var("OPENSKYRIM_VALIDATE_LIMIT")',
        'env::var("MUDCRAB_VALIDATE_LIMIT").or_else(|_| env::var("OPENSKYRIM_VALIDATE_LIMIT"))',
    ),
    ("collision key", "a.md", "`openSkyrimCollision`", "`mudcrabCollision`"),
    (
        "collision reader",
        "a.rs",
        'value.get("openSkyrimCollision")',
        'value.get("mudcrabCollision").or_else(|| value.get("openSkyrimCollision"))',
    ),
    (
        "repo URL and clone",
        "a.md",
        "github.com/realfakenerd/OpenSkyrim, realfakenerd/wah-krah-jol, cd wah-krah-jol",
        "github.com/Mudcrab-Team/mudcrab, Mudcrab-Team/mudcrab, cd mudcrab",
    ),
    ("gitignore entry", ".gitignore", "a\nopenskyrim.cfg\nb\n", "a\nb\n"),
    ("keep OPEN_SKYRIM_material", "a.rs", '"OPEN_SKYRIM_material"', '"OPEN_SKYRIM_material"'),
    ("keep openSkyrim key", "a.rs", '"openSkyrim": 1, "/openSkyrim/x"', '"openSkyrim": 1, "/openSkyrim/x"'),
    ("keep contact", "a.md", "openskyrimdev@gmail.com", "openskyrimdev@gmail.com"),
    ("keep ko_fi", "a.yml", "ko_fi: wahkrahjol", "ko_fi: wahkrahjol"),
    ("keep contributor path", "a.md", "/home/dev/.cache/openskyrim/x", "/home/dev/.cache/openskyrim/x"),
    ("keep contributor folder", "a.md", "C:\\OpenSkyrim-lod", "C:\\OpenSkyrim-lod"),
    ("keep old-name docs", "a.md", "the old OPENSKYRIM_* names", "the old OPENSKYRIM_* names"),
]


def self_test():
    failed = 0
    for name, path, text, expected in SELF_TESTS:
        got, _count = rewrite(path, text)
        again, _count = rewrite(path, got)
        if got != expected or again != got:
            failed += 1
            print(f"FAIL {name}: got {got!r}, rerun {again!r}, expected {expected!r}", file=sys.stderr)
    print(f"{len(SELF_TESTS) - failed}/{len(SELF_TESTS)} self-tests passed")
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument(
        "--check", action="store_true", help="list files still to rename, change nothing; exit 1 if any"
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the rules on inline samples, touch no files"
    )
    args = parser.parse_args()
    if args.self_test:
        return self_test()

    root = repo_root()
    pending = []
    for name in tracked_files(root):
        if name == SELF or name.startswith("vendor/"):
            continue
        path = root / name
        if not path.is_file():
            continue
        data = path.read_bytes()
        if b"\x00" in data:
            continue
        try:
            text = data.decode("utf-8")
        except UnicodeDecodeError:
            continue
        new_text, count = rewrite(name, text)
        if new_text == text:
            continue
        pending.append((name, count))
        if not args.check:
            path.write_bytes(new_text.encode("utf-8"))

    for name, count in pending:
        print(f"{name}: {count}")
    if args.check:
        if pending:
            print(f"{len(pending)} file(s) still to rename", file=sys.stderr)
            return 1
        print("nothing left to rename")
        return 0
    print(f"{len(pending)} file(s) changed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
