#!/usr/bin/env python3
"""注释双语审计 / Bilingual comment audit.

扫描全行注释与 doc 注释，按块判断「仅中文 / 仅英文 / 已双语」，并支持
基线棘轮门禁：存量允许，新增单语注释失败。
Scans full-line and doc comments, classifies each block as zh-only / en-only /
bilingual, and supports a baseline ratchet: existing debt is allowed, new
monolingual comments fail.

用法 / Usage:
  python3 scripts/comment_language_audit.py                 # 人类可读摘要
  python3 scripts/comment_language_audit.py --json          # 机器可读
  python3 scripts/comment_language_audit.py --update-baseline \
      --baseline scripts/comment_baseline.json              # 重建基线
  python3 scripts/comment_language_audit.py --gate \
      --baseline scripts/comment_baseline.json              # CI 门禁

口径 / Scope:
- 全行注释 + Rust/Java/C#/JSDoc doc 注释 + Python docstring；
  行尾注释、Markdown、生成/第三方代码不计。
- 「双语」= 同一注释块内同时出现中文与英文（含 `EN:` 标记）；是覆盖率上限，
  不保证逐句翻译。 / "Bilingual" means both languages appear in one block;
  it is an upper bound, not sentence-level completeness.
"""

from __future__ import annotations

import argparse
import ast
import io
import json
import os
import re
import sys
import tokenize
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path

ROOT_DEFAULT = Path(__file__).resolve().parent.parent
CJK = re.compile(r"[\u4e00-\u9fff]")
WORD = re.compile(r"[A-Za-z][A-Za-z'_-]+")
EN_MARKER = re.compile(r"(?:\bEN\b|\bEnglish\b)\s*:", re.I)
# 工具指令不是承载语义的注释：noqa / type: / pragma / fmt / shellcheck 等。
# EN: Tool directives (noqa, type:, pragma, fmt, shellcheck, ...) are not
# semantic comments and are skipped.
TOOL_DIRECTIVE = re.compile(
    r"^(?:noqa|type:|pragma:|fmt:|isort:|ruff:|mypy:|pylint:|shellcheck\b)",
    re.I,
)
LEGAL = re.compile(
    r"Licensed under the Apache License|SPDX-License-Identifier|"
    r"Copyright \(c\)|Apache License, Version 2\.0",
    re.I,
)
EXTS = {".rs", ".py", ".ts", ".tsx", ".js", ".mjs", ".go", ".java", ".cs",
        ".sh", ".bash", ".c", ".h"}
SKIP_DIRS = {".git", "target", "dist", "node_modules", ".venv", "__pycache__",
             ".next", "build", "obj", "bin", ".gradle", ".idea", "logs",
             "storage", "snapshots", "corpus-gen", "artifacts"}


@dataclass
class Block:
    path: str
    start: int
    lines: list[str]
    doc: bool = False
    kind: str = ""

    def classify(self) -> str:
        text = "\n".join(self.lines)
        if LEGAL.search(text):
            return "legal"
        zh = [ln for ln in self.lines if CJK.search(ln)]
        en = [
            ln for ln in self.lines
            if not CJK.search(ln) and len(WORD.findall(ln)) >= 2
        ]
        if not zh and not en:
            return "neutral"
        if zh and (en or EN_MARKER.search(text)):
            return "bilingual"
        if zh:
            return "zh_only"
        return "en_only"


def c_like_blocks(path: str, text: str) -> list[Block]:
    lines = text.splitlines()
    blocks: list[Block] = []
    i = 0
    while i < len(lines):
        s = lines[i].strip()
        if s.startswith("//"):
            start = i + 1
            doc = s.startswith("///") or s.startswith("//!")
            body = []
            while i < len(lines) and lines[i].strip().startswith("//"):
                body.append(lines[i].strip().lstrip("/").strip())
                i += 1
            blocks.append(Block(path, start, body, doc))
            continue
        # 只把行首块注释当作全行注释；行中 `/* ... */`（如 SQL 字符串）不计。
        # EN: Only line-leading block comments count; inline /* ... */ is out of scope.
        if s.startswith("/*"):
            start = i + 1
            doc = s.startswith("/**") or s.startswith("/*!")
            body = []
            while i < len(lines):
                seg = lines[i]
                if "/*" in seg:
                    seg = seg.split("/*", 1)[1]
                end = "*/" in seg
                if end:
                    seg = seg.split("*/", 1)[0]
                seg = seg.strip().lstrip("*").strip()
                if seg:
                    body.append(seg)
                i += 1
                if end:
                    break
            blocks.append(Block(path, start, body, doc))
            continue
        i += 1
    return blocks


def python_blocks(path: str, text: str) -> list[Block]:
    blocks: list[Block] = []
    comments: list[tuple[int, str]] = []
    try:
        for tok in tokenize.generate_tokens(io.StringIO(text).readline):
            if tok.type == tokenize.COMMENT:
                body = tok.string.lstrip("#").strip()
                # 跳过 shebang（`#!/usr/bin/env ...`），它不是双语注释。
                # Skip the shebang; it is not a semantic comment.
                if (
                    body
                    and not body.startswith("!")
                    and not TOOL_DIRECTIVE.match(body)
                ):
                    comments.append((tok.start[0], body))
    except Exception:
        pass
    comments.sort()
    i = 0
    while i < len(comments):
        start, body = comments[i]
        group = [body]
        last = start
        i += 1
        while i < len(comments) and comments[i][0] == last + 1:
            last = comments[i][0]
            group.append(comments[i][1])
            i += 1
        blocks.append(Block(path, start, group, doc=False))
    try:
        tree = ast.parse(text)
    except Exception:
        return blocks
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef,
                                 ast.AsyncFunctionDef)):
            continue
        if not node.body:
            continue
        first = node.body[0]
        if not (isinstance(first, ast.Expr)
                and isinstance(first.value, ast.Constant)
                and isinstance(first.value.value, str)):
            continue
        ds = first.value.value
        lines = [ln.strip() for ln in ds.splitlines() if ln.strip()]
        if lines:
            blocks.append(Block(path, first.lineno, lines, doc=True))
    return blocks


HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")


def shell_blocks(path: str, text: str) -> list[Block]:
    blocks: list[Block] = []
    lines = text.splitlines()
    i = 0
    skip_until = None
    while i < len(lines):
        raw = lines[i]
        s = raw.strip()
        # heredoc 正文是命令输出/生成的 Markdown，不是 shell 注释。
        # EN: heredoc bodies are program output/generated Markdown, not comments.
        if skip_until is not None:
            if s == skip_until:
                skip_until = None
            i += 1
            continue
        if s.startswith("#") and not s.startswith("#!"):
            start = i + 1
            body = []
            while i < len(lines):
                cur = lines[i].strip()
                if not cur.startswith("#") or cur.startswith("#!"):
                    break
                text_body = cur.lstrip("#").strip()
                if not TOOL_DIRECTIVE.match(text_body):
                    body.append(text_body)
                i += 1
            if any(body):
                blocks.append(Block(path, start, body, doc=False))
            continue
        m = HEREDOC.search(s)
        if m:
            skip_until = m.group(2)
        i += 1
    return blocks


def area_of(rel: str) -> str:
    parts = rel.split(os.sep)
    if parts[0].startswith("houyi-"):
        return parts[0]
    if parts[0] == "sdk" and len(parts) > 1:
        return "sdk/" + parts[1]
    return parts[0]


def collect(root: Path) -> tuple[list[Block], int]:
    blocks: list[Block] = []
    scanned = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [
            d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")
        ]
        for name in filenames:
            ext = os.path.splitext(name)[1]
            if ext not in EXTS:
                continue
            path = Path(dirpath) / name
            rel = str(path.relative_to(root))
            try:
                if path.stat().st_size > 2 * 1024 * 1024:
                    continue
                raw = path.read_bytes()
                if b"\x00" in raw:
                    continue
                text = raw.decode("utf-8", errors="ignore")
            except OSError:
                continue
            scanned += 1
            if ext == ".py":
                found = python_blocks(rel, text)
            elif ext in (".sh", ".bash"):
                found = shell_blocks(rel, text)
            else:
                found = c_like_blocks(rel, text)
            blocks.extend(found)
    return blocks, scanned


def summarize(blocks: list[Block]) -> dict:
    totals: Counter = Counter()
    by_area: dict[str, Counter] = defaultdict(Counter)
    doc_totals: Counter = Counter()
    per_file: dict[str, Counter] = defaultdict(Counter)
    for block in blocks:
        kind = block.classify()
        block.kind = kind
        n = len(block.lines)
        totals[kind] += n
        by_area[area_of(block.path)][kind] += n
        per_file[block.path][kind] += n
        if block.doc:
            doc_totals[kind] += n
    denom = totals["zh_only"] + totals["en_only"] + totals["bilingual"]
    coverage = (totals["bilingual"] / denom) if denom else 1.0
    return {
        "scanned_blocks": len(blocks),
        "totals": dict(totals),
        "coverage": round(coverage, 4),
        "doc_totals": dict(doc_totals),
        "by_area": {k: dict(v) for k, v in sorted(by_area.items())},
        "per_file": {k: dict(v) for k, v in sorted(per_file.items())},
    }


def baseline_from(summary: dict) -> dict:
    files = {}
    for path, counts in summary["per_file"].items():
        zh = counts.get("zh_only", 0)
        en = counts.get("en_only", 0)
        if zh or en:
            files[path] = {"zh_only": zh, "en_only": en}
    return {"version": 1, "files": files}


def print_summary(summary: dict) -> None:
    totals = summary["totals"]
    print(f"scanned blocks: {summary['scanned_blocks']}")
    print("\n== comment lines ==")
    for k in ("bilingual", "zh_only", "en_only", "neutral", "legal"):
        print(f"{k:10s} {totals.get(k, 0):6d}")
    print(f"coverage: {summary['coverage']:.1%} bilingual")
    print("\n== by area (lines) ==")
    rows = []
    for area, counts in summary["by_area"].items():
        missing = counts.get("zh_only", 0) + counts.get("en_only", 0)
        if missing:
            rows.append((missing, area, counts))
    for _, area, counts in sorted(rows, reverse=True):
        print(
            f"{area:22s} zh_only={counts.get('zh_only', 0):5d} "
            f"en_only={counts.get('en_only', 0):4d} "
            f"bilingual={counts.get('bilingual', 0):5d}"
        )
    print("\n== doc comments (lines) ==")
    for k in ("bilingual", "zh_only", "en_only", "neutral", "legal"):
        print(f"{k:10s} {summary['doc_totals'].get(k, 0):6d}")


def gate(summary: dict, baseline: dict) -> int:
    allowed = baseline.get("files", {})
    failures = []
    for path, counts in summary["per_file"].items():
        zh = counts.get("zh_only", 0)
        en = counts.get("en_only", 0)
        old = allowed.get(path, {})
        limit_zh = old.get("zh_only", 0)
        limit_en = old.get("en_only", 0)
        if zh > limit_zh or en > limit_en:
            failures.append((path, zh, limit_zh, en, limit_en))
    if not failures:
        print("comment language gate: PASS（无新增单语注释）")
        return 0
    print("comment language gate: FAIL（存在新增/增加的单语注释）", file=sys.stderr)
    for path, zh, lzh, en, len_ in failures:
        print(
            f"  {path}: zh_only {lzh}->{zh}, en_only {len_}->{en}",
            file=sys.stderr,
        )
    print(
        "  修复：为新增注释补中英双语，或更新基线（仅允许递减）。\n"
        "  Fix: add both languages, or update the baseline (decreases only).",
        file=sys.stderr,
    )
    return 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT_DEFAULT)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--gate", action="store_true")
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--update-baseline", action="store_true")
    args = parser.parse_args()

    blocks, _ = collect(args.root.resolve())
    summary = summarize(blocks)

    if args.update_baseline:
        if not args.baseline:
            print("!! --update-baseline requires --baseline <path>", file=sys.stderr)
            return 2
        baseline = baseline_from(summary)
        args.baseline.write_text(
            json.dumps(baseline, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(f"baseline written: {args.baseline}")
        return 0

    if args.json:
        print(json.dumps(summary, ensure_ascii=False, indent=2, sort_keys=True))
        return 0

    print_summary(summary)

    if args.gate:
        if not args.baseline:
            print("!! --gate requires --baseline <path>", file=sys.stderr)
            return 2
        baseline = json.loads(args.baseline.read_text(encoding="utf-8"))
        return gate(summary, baseline)
    return 0


if __name__ == "__main__":
    sys.exit(main())
