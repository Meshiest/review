"""Counts color, corner-radius and font-family literals outside the theme modules.

    python scripts/lint_literals.py [paths...]

Views take colors from the theme and fonts from the interface or buffer font
settings, so these must not appear in them:

    rgb(  rgba(  hsla(  0x + six hex digits  rounded(px(  font_family("

A theme module is a file named `theme.rs` or any file under a `theme/`
directory; those are skipped. Comments are ignored. Prints each hit as
`file:line: pattern: code`, a count per scanned root, then the total, and
exits 1 when the total is not 0.
"""
import os
import re
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..'))
DEFAULT_PATHS = ['crates/app', 'crates/diff_view/src', 'crates/term_view/src', 'crates/log_view/src']

PATTERNS = [
    ('rgb(', re.compile(r'\brgb\(')),
    ('rgba(', re.compile(r'\brgba\(')),
    ('hsla(', re.compile(r'\bhsla\(')),
    ('0x six-hex', re.compile(r'\b0x[0-9a-fA-F]{6}(?![0-9a-fA-F_])')),
    ('rounded(px(', re.compile(r'\brounded\(\s*px\(')),
    ('font_family("', re.compile(r'\bfont_family\(\s*"')),
]


def is_theme_module(path):
    parts = os.path.normpath(path).split(os.sep)
    return parts[-1] == 'theme.rs' or 'theme' in parts[:-1]


def strip_comment(line):
    """The line without a trailing `//` comment (one outside a string)."""
    in_string = False
    i = 0
    while i < len(line):
        ch = line[i]
        if ch == '\\' and in_string:
            i += 2
            continue
        if ch == '"':
            in_string = not in_string
        elif not in_string and line.startswith('//', i):
            return line[:i]
        i += 1
    return line


def rust_files(root):
    if os.path.isfile(root):
        yield root
        return
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d != 'target')
        for name in sorted(filenames):
            if name.endswith('.rs'):
                yield os.path.join(dirpath, name)


def scan(path):
    hits = []
    in_block = False
    with open(path, encoding='utf-8') as f:
        for number, line in enumerate(f, 1):
            code = line
            if in_block:
                end = code.find('*/')
                if end < 0:
                    continue
                code = code[end + 2:]
                in_block = False
            start = code.find('/*')
            if start >= 0 and '*/' not in code[start:]:
                code = code[:start]
                in_block = True
            code = strip_comment(code)
            for name, pattern in PATTERNS:
                for _ in pattern.finditer(code):
                    hits.append((number, name, line.strip()))
    return hits


def main(argv):
    roots = argv or DEFAULT_PATHS
    total = 0
    per_root = []
    for root in roots:
        full = root if os.path.isabs(root) else os.path.join(ROOT, root)
        count = 0
        for path in rust_files(full):
            if is_theme_module(path):
                continue
            rel = os.path.relpath(path, ROOT).replace(os.sep, '/')
            for number, name, text in scan(path):
                print(f'{rel}:{number}: {name}: {text}')
                count += 1
        per_root.append((root, count))
        total += count
    print()
    for root, count in per_root:
        print(f'{root}: {count}')
    print(f'total: {total}')
    return 1 if total else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
