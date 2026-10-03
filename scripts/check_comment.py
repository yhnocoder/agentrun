import re
import subprocess
import sys
from pathlib import Path

WHY = re.compile(r"//\s*why\(#\d+\): \S")
RAW_STRING = re.compile(r"(?:br|cr|r)(#*)\"")
CHAR = re.compile(r"'(?:[^'\\\n]|\\(?:[nrt0\\'\"]|x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}))'")


def comments(source):
    found = []
    i = 0
    line = 1
    n = len(source)
    while i < n:
        c = source[i]
        if c == "\n":
            line += 1
            i += 1
        elif source.startswith("//", i):
            end = source.find("\n", i)
            end = n if end == -1 else end
            found.append((line, source[i:end]))
            i = end
        elif source.startswith("/*", i):
            start_line = line
            depth = 0
            j = i
            while j < n:
                if source.startswith("/*", j):
                    depth += 1
                    j += 2
                elif source.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            text = source[i:j]
            found.append((start_line, text.splitlines()[0]))
            line += text.count("\n")
            i = j
        elif (raw := RAW_STRING.match(source, i)) and not is_identifier_char(source, i - 1):
            closing = '"' + raw.group(1)
            end = source.find(closing, raw.end())
            end = n if end == -1 else end + len(closing)
            line += source.count("\n", i, end)
            i = end
        elif c == '"':
            j = i + 1
            while j < n and source[j] != '"':
                j += 2 if source[j] == "\\" else 1
            end = min(j + 1, n)
            line += source.count("\n", i, end)
            i = end
        elif c == "'" and (char := CHAR.match(source, i)):
            i = char.end()
        else:
            i += 1
    return found


def is_identifier_char(source, index):
    return index >= 0 and (source[index].isalnum() or source[index] == "_")


def rust_files(args):
    if args:
        return [Path(arg) for arg in args]
    listed = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return [Path(path) for path in listed if Path(path).exists()]


def main():
    violations = []
    for path in rust_files(sys.argv[1:]):
        for line, text in comments(path.read_text(encoding="utf-8")):
            if not WHY.match(text):
                violations.append(f"{path}:{line}: {text.strip()}")
    for violation in violations:
        print(violation)
    if violations:
        print("comments are not allowed; see CLAUDE.md, the only exception is // why(#N): reason")
    sys.exit(1 if violations else 0)


if __name__ == "__main__":
    main()
