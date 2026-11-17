#!/usr/bin/env python3
"""Deterministic coding style and architecture gate checker for Rust crates."""

from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Tuple

SKIP_DIRS = {".cache", ".runtime", ".git", "target"}
CJK_RE = re.compile(r"[\u4e00-\u9fff]")
MAX_EFFECTIVE_LINES = 1000
MAX_DIR_FILES = 15
MAX_TYPES_PER_FILE = 3
MAX_FN_PARAMS = 6

TYPE_KEYWORDS = {"struct", "enum", "trait", "union"}
IGNORE_PRE_TYPE_TOKENS = {"pub", "unsafe", "crate", "super", "in"}


@dataclass
class Token:
    kind: str  # "ident", "op", "punct"
    value: str
    line: int
    col: int


@dataclass
class CommentEntry:
    kind: str  # "//", "///", "//!", "/*"
    start_line: int
    end_line: int
    trailing_on_code: bool
    raw_lines: List[str]


@dataclass
class Violation:
    path: Path
    line: int
    rule: str
    detail: str


def has_cjk(text: str) -> bool:
    return bool(CJK_RE.search(text))


def check_last_comment_punctuation(text_lines: List[str]) -> bool:
    non_empty = [line.rstrip() for line in text_lines if line.strip()]
    if not non_empty:
        return True
    last = non_empty[-1]
    return not (
        last.endswith("。")
        or last.endswith(".")
        or last.endswith("。”")
        or last.endswith('."')
    )


def strip_line_comment_prefix(raw: str) -> str:
    stripped = raw.lstrip()
    if stripped.startswith("///") or stripped.startswith("//!"):
        return stripped[3:]
    if stripped.startswith("//"):
        return stripped[2:]
    return stripped


def strip_block_comment_lines(raw: str) -> List[str]:
    inner = raw
    if inner.startswith("/**") and not inner.startswith("/***"):
        inner = inner[3:]
    elif inner.startswith("/*!"):
        inner = inner[3:]
    elif inner.startswith("/*"):
        inner = inner[2:]
    if inner.endswith("*/"):
        inner = inner[:-2]
    cleaned: List[str] = []
    for line in inner.splitlines():
        s = line.strip()
        if s.startswith("*"):
            s = s[1:].lstrip()
        cleaned.append(s)
    return cleaned


def scan_rust_file(source: str) -> Tuple[List[Token], List[CommentEntry], set[int]]:
    """Lex Rust source into code tokens, comments, and lines containing non-comment code."""
    n = len(source)
    i = 0
    line = 1
    col = 1
    tokens: List[Token] = []
    comments: List[CommentEntry] = []
    code_lines: set[int] = set()

    def advance(count: int = 1) -> None:
        nonlocal i, line, col
        for _ in range(count):
            if i >= n:
                return
            ch = source[i]
            i += 1
            if ch == "\n":
                line += 1
                col = 1
            else:
                col += 1

    def mark_code_line(ln: int) -> None:
        code_lines.add(ln)

    while i < n:
        ch = source[i]

        # Whitespace
        if ch in (" ", "\t", "\r", "\n"):
            advance()
            continue

        # Line comment
        if source.startswith("//", i):
            start_ln = line
            trailing = start_ln in code_lines
            start_idx = i
            while i < n and source[i] != "\n":
                advance()
            raw = source[start_idx:i]
            if raw.startswith("///") and not raw.startswith("////"):
                kind = "///"
            elif raw.startswith("//!"):
                kind = "//!"
            else:
                kind = "//"
            comments.append(
                CommentEntry(
                    kind=kind,
                    start_line=start_ln,
                    end_line=start_ln,
                    trailing_on_code=trailing,
                    raw_lines=[raw],
                )
            )
            continue

        # Block comment (supports Rust nesting)
        if source.startswith("/*", i):
            start_ln = line
            trailing = start_ln in code_lines
            start_idx = i
            depth = 0
            while i < n:
                if source.startswith("/*", i):
                    depth += 1
                    advance(2)
                elif source.startswith("*/", i):
                    depth -= 1
                    advance(2)
                    if depth == 0:
                        break
                else:
                    advance()
            end_ln = line
            raw = source[start_idx:i]
            comments.append(
                CommentEntry(
                    kind="/*",
                    start_line=start_ln,
                    end_line=end_ln,
                    trailing_on_code=trailing,
                    raw_lines=raw.splitlines() or [raw],
                )
            )
            continue

        # Raw string literals: r#"..."#, br#"..."#, cr#"..."#
        raw_match = None
        for prefix in ("br", "cr", "r"):
            if source.startswith(prefix, i):
                j = i + len(prefix)
                hashes = 0
                while j < n and source[j] == "#":
                    hashes += 1
                    j += 1
                if j < n and source[j] == '"':
                    raw_match = (len(prefix), hashes)
                    break
        if raw_match is not None:
            pref_len, hashes = raw_match
            tok_ln, tok_col = line, col
            mark_code_line(line)
            advance(pref_len + hashes + 1)
            closing = '"' + ("#" * hashes)
            while i < n:
                mark_code_line(line)
                if source.startswith(closing, i):
                    advance(len(closing))
                    break
                advance()
            tokens.append(Token("literal", "str", tok_ln, tok_col))
            continue

        # Byte / C / normal string literals
        str_prefix = None
        for prefix in ("b\"", "c\"", "\""):
            if source.startswith(prefix, i):
                str_prefix = len(prefix)
                break
        if str_prefix is not None:
            tok_ln, tok_col = line, col
            mark_code_line(line)
            advance(str_prefix)
            while i < n:
                mark_code_line(line)
                c = source[i]
                if c == "\\":
                    advance(2 if i + 1 < n else 1)
                elif c == '"':
                    advance()
                    break
                else:
                    advance()
            tokens.append(Token("literal", "str", tok_ln, tok_col))
            continue

        # Byte char literal b'...'
        if source.startswith("b'", i):
            tok_ln, tok_col = line, col
            mark_code_line(line)
            advance(2)
            while i < n:
                c = source[i]
                if c == "\\":
                    advance(2 if i + 1 < n else 1)
                elif c == "'":
                    advance()
                    break
                elif c == "\n":
                    break
                else:
                    advance()
            tokens.append(Token("literal", "char", tok_ln, tok_col))
            continue

        # Char literal vs lifetime
        if ch == "'":
            if i + 1 < n and source[i + 1] == "\\":
                tok_ln, tok_col = line, col
                mark_code_line(line)
                advance(2)
                while i < n:
                    c = source[i]
                    if c == "\\":
                        advance(2 if i + 1 < n else 1)
                    elif c == "'":
                        advance()
                        break
                    elif c == "\n":
                        break
                    else:
                        advance()
                tokens.append(Token("literal", "char", tok_ln, tok_col))
                continue
            if i + 2 < n and source[i + 2] == "'" and source[i + 1] not in ("'", "\n"):
                tok_ln, tok_col = line, col
                mark_code_line(line)
                advance(3)
                tokens.append(Token("literal", "char", tok_ln, tok_col))
                continue
            # Lifetime or label
            tok_ln, tok_col = line, col
            mark_code_line(line)
            advance()
            start = i
            while i < n and (source[i].isalnum() or source[i] == "_"):
                advance()
            tokens.append(Token("lifetime", "'" + source[start:i], tok_ln, tok_col))
            continue

        # Raw identifier r#ident
        if source.startswith("r#", i) and i + 2 < n and (source[i + 2].isalpha() or source[i + 2] == "_"):
            tok_ln, tok_col = line, col
            mark_code_line(line)
            advance(2)
            start = i
            while i < n and (source[i].isalnum() or source[i] == "_"):
                advance()
            tokens.append(Token("raw_ident", source[start:i], tok_ln, tok_col))
            continue

        # Identifier / keyword / number
        if ch.isalpha() or ch == "_":
            tok_ln, tok_col = line, col
            mark_code_line(line)
            start = i
            while i < n and (source[i].isalnum() or source[i] == "_"):
                advance()
            tokens.append(Token("ident", source[start:i], tok_ln, tok_col))
            continue

        if ch.isdigit():
            tok_ln, tok_col = line, col
            mark_code_line(line)
            start = i
            while i < n and (source[i].isalnum() or source[i] == "_"):
                advance()
            tokens.append(Token("literal", source[start:i], tok_ln, tok_col))
            continue

        # Multi-char operators (must precede single-char '<', '>', etc.; keep '<' and '>' separate for nested generics '>>')
        tok_ln, tok_col = line, col
        mark_code_line(line)
        matched_multi = None
        for op in ("->", "=>", "::", "<=", ">=", "==", "!=", "..=", ".."):
            if source.startswith(op, i):
                matched_multi = op
                break
        if matched_multi is not None:
            advance(len(matched_multi))
            tokens.append(Token("op", matched_multi, tok_ln, tok_col))
            continue

        # Single char punctuation / operator
        advance()
        tokens.append(Token("punct", ch, tok_ln, tok_col))

    return tokens, comments, code_lines


def build_comment_blocks(comments: List[CommentEntry]) -> List[CommentEntry]:
    blocks: List[CommentEntry] = []
    for entry in comments:
        if entry.kind == "/*":
            blocks.append(entry)
            continue
        if (
            blocks
            and not entry.trailing_on_code
            and not blocks[-1].trailing_on_code
            and blocks[-1].kind == entry.kind
            and entry.start_line == blocks[-1].end_line + 1
        ):
            blocks[-1].end_line = entry.end_line
            blocks[-1].raw_lines.extend(entry.raw_lines)
        else:
            blocks.append(
                CommentEntry(
                    kind=entry.kind,
                    start_line=entry.start_line,
                    end_line=entry.end_line,
                    trailing_on_code=entry.trailing_on_code,
                    raw_lines=list(entry.raw_lines),
                )
            )
    return blocks


def extract_comment_text_lines(block: CommentEntry) -> List[str]:
    if block.kind == "/*":
        return strip_block_comment_lines("\n".join(block.raw_lines))
    return [strip_line_comment_prefix(ln) for ln in block.raw_lines]


def find_type_declarations(tokens: List[Token]) -> List[Tuple[int, str, str]]:
    """Return list of (line, kind, name) for struct/enum/trait/union declarations."""
    decls: List[Tuple[int, str, str]] = []
    idx = 0
    total = len(tokens)
    while idx < total:
        tok = tokens[idx]
        if tok.kind == "ident" and tok.value in TYPE_KEYWORDS:
            # Avoid `use x::struct;` or field access `.union`
            prev_tok = tokens[idx - 1] if idx > 0 else None
            if prev_tok is not None and prev_tok.value in (".", "::"):
                idx += 1
                continue
            if idx + 1 < total and tokens[idx + 1].kind == "ident":
                name_tok = tokens[idx + 1]
                # Contextual keyword `union`: verify followed by `{`, `<`, or `where`
                if tok.value == "union":
                    if idx + 2 < total and tokens[idx + 2].value not in ("{", "<", "where"):
                        idx += 1
                        continue
                # Also allow `struct Name;` / `struct Name(...)` / `struct Name<...>` / `struct Name {`
                if tok.value in ("struct", "enum", "trait"):
                    # Ensure not preceded by `=` or `:` or `,` (weird macro fragment)
                    if prev_tok is not None and prev_tok.value in (":", "=", ",", "("):
                        idx += 1
                        continue
                # Find the start line of any `pub` / `unsafe` modifiers right before it
                decl_line = tok.line
                back = idx - 1
                while back >= 0:
                    bt = tokens[back]
                    if bt.kind == "ident" and bt.value in IGNORE_PRE_TYPE_TOKENS:
                        decl_line = bt.line
                        back -= 1
                        continue
                    if bt.value == ")":
                        # Skip balanced `pub(crate)` or `pub(in a::b)` backwards
                        p_depth = 1
                        k = back - 1
                        while k >= 0 and p_depth > 0:
                            if tokens[k].value == ")":
                                p_depth += 1
                            elif tokens[k].value == "(":
                                p_depth -= 1
                            k -= 1
                        if k >= 0 and tokens[k].kind == "ident" and tokens[k].value == "pub":
                            decl_line = tokens[k].line
                            back = k - 1
                            continue
                    break
                decls.append((decl_line, tok.value, name_tok.value))
        idx += 1
    return decls


def find_attribute_line_ranges(tokens: List[Token]) -> List[Tuple[int, int]]:
    """Return (start_line, end_line) spans of `#[...]` and `#![...]` attributes."""
    ranges: List[Tuple[int, int]] = []
    idx = 0
    total = len(tokens)
    while idx < total - 1:
        if tokens[idx].value == "#":
            bracket_idx = idx + 1
            if bracket_idx < total and tokens[bracket_idx].value == "!":
                bracket_idx += 1
            if bracket_idx < total and tokens[bracket_idx].value == "[":
                start_ln = tokens[idx].line
                depth = 1
                j = bracket_idx + 1
                while j < total and depth > 0:
                    if tokens[j].value == "[":
                        depth += 1
                    elif tokens[j].value == "]":
                        depth -= 1
                    j += 1
                end_ln = tokens[j - 1].line if j - 1 < total else start_ln
                ranges.append((start_ln, end_ln))
                idx = j
                continue
        idx += 1
    return ranges


def check_type_has_chinese_doc(
    decl_line: int,
    blocks: List[CommentEntry],
    attr_ranges: List[Tuple[int, int]],
    source_lines: List[str],
) -> Optional[str]:
    """Verify a struct/enum/trait/union has contiguous /// doc block with Chinese directly above."""
    cursor = decl_line - 1
    # Walk upwards across contiguous `#[...]` attribute blocks and whitespace inside those attrs
    while cursor >= 1:
        matched_attr = False
        for a_start, a_end in attr_ranges:
            if a_start <= cursor <= a_end:
                cursor = a_start - 1
                matched_attr = True
                break
        if matched_attr:
            continue
        if source_lines[cursor - 1].strip() == "":
            # Blank line immediately before type/attr breaks contiguity
            break
        break

    # Now `cursor` must be the `end_line` of a `///` (or doc `/*`) comment block
    for block in blocks:
        if block.end_line == cursor and block.kind in ("///", "/*"):
            if block.kind == "/*" and not (
                block.raw_lines[0].lstrip().startswith("/**")
            ):
                continue
            text = "\n".join(extract_comment_text_lines(block))
            if not has_cjk(text):
                return "数据结构文档注释必须包含简体中文"
            return None
    return "数据结构上方缺少紧邻的简体中文 /// 文档注释"


def count_fn_signatures(tokens: List[Token]) -> List[Tuple[int, str, int]]:
    """Return list of (line, fn_name, param_count) for non-extern `fn <ident>` definitions."""
    results: List[Tuple[int, str, int]] = []
    idx = 0
    total = len(tokens)
    while idx < total:
        tok = tokens[idx]
        # Skip `extern "<abi>" { ... }` and `extern { ... }` FFI declaration blocks
        if tok.kind == "ident" and tok.value == "extern":
            prev_tok = tokens[idx - 1] if idx > 0 else None
            if prev_tok is None or prev_tok.value not in (".", "::"):
                block_open = None
                if idx + 1 < total and tokens[idx + 1].value == "{":
                    block_open = idx + 1
                elif (
                    idx + 2 < total
                    and tokens[idx + 1].kind == "literal"
                    and tokens[idx + 1].value == "str"
                    and tokens[idx + 2].value == "{"
                ):
                    block_open = idx + 2
                if block_open is not None:
                    brace_depth = 1
                    cur = block_open + 1
                    while cur < total and brace_depth > 0:
                        if tokens[cur].value == "{":
                            brace_depth += 1
                        elif tokens[cur].value == "}":
                            brace_depth -= 1
                        cur += 1
                    idx = cur
                    continue
        if tok.kind == "ident" and tok.value == "fn":
            prev_tok = tokens[idx - 1] if idx > 0 else None
            if prev_tok is not None and prev_tok.value in (".", "::"):
                idx += 1
                continue
            is_extern_fn = (
                (idx >= 1 and tokens[idx - 1].kind == "ident" and tokens[idx - 1].value == "extern")
                or (
                    idx >= 2
                    and tokens[idx - 2].kind == "ident"
                    and tokens[idx - 2].value == "extern"
                    and tokens[idx - 1].kind == "literal"
                    and tokens[idx - 1].value == "str"
                )
            )
            if idx + 1 >= total or tokens[idx + 1].kind not in ("ident", "raw_ident"):
                idx += 1
                continue
            fn_name = tokens[idx + 1].value
            fn_line = tok.line
            cur = idx + 2
            # Skip optional generic parameter list `<...>`
            if cur < total and tokens[cur].value == "<":
                angle_depth = 1
                cur += 1
                while cur < total and angle_depth > 0:
                    v = tokens[cur].value
                    if v == "<":
                        angle_depth += 1
                    elif v == ">":
                        angle_depth -= 1
                    elif v == "(":
                        # Skip balanced `(...)` inside `Fn(A, B) -> C`
                        p_depth = 1
                        cur += 1
                        while cur < total and p_depth > 0:
                            if tokens[cur].value == "(":
                                p_depth += 1
                            elif tokens[cur].value == ")":
                                p_depth -= 1
                            cur += 1
                        continue
                    cur += 1
            if cur < total and tokens[cur].value == "(":
                p_depth = 1
                a_depth = 0
                b_depth = 0
                brace_depth = 0
                cur += 1
                param_tokens = 0
                param_count = 0
                while cur < total and p_depth > 0:
                    v = tokens[cur].value
                    if v == "(":
                        p_depth += 1
                        param_tokens += 1
                    elif v == ")":
                        p_depth -= 1
                        if p_depth == 0:
                            if param_tokens > 0:
                                param_count += 1
                            break
                        param_tokens += 1
                    elif p_depth == 1:
                        if v == "<":
                            a_depth += 1
                            param_tokens += 1
                        elif v == ">" and a_depth > 0:
                            a_depth -= 1
                            param_tokens += 1
                        elif v == "[":
                            b_depth += 1
                            param_tokens += 1
                        elif v == "]" and b_depth > 0:
                            b_depth -= 1
                            param_tokens += 1
                        elif v == "{":
                            brace_depth += 1
                            param_tokens += 1
                        elif v == "}" and brace_depth > 0:
                            brace_depth -= 1
                            param_tokens += 1
                        elif v == "," and a_depth == 0 and b_depth == 0 and brace_depth == 0:
                            if param_tokens > 0:
                                param_count += 1
                            param_tokens = 0
                        else:
                            param_tokens += 1
                    cur += 1
                if not is_extern_fn:
                    results.append((fn_line, fn_name, param_count))
                idx = cur + 1
                continue
        idx += 1
    return results


def check_file(path: Path) -> Tuple[List[Violation], dict]:
    source = path.read_text(encoding="utf-8")
    source_lines = source.splitlines()
    tokens, comments, code_lines = scan_rust_file(source)
    blocks = build_comment_blocks(comments)
    decls = find_type_declarations(tokens)
    attr_ranges = find_attribute_line_ranges(tokens)
    fns = count_fn_signatures(tokens)

    violations: List[Violation] = []

    effective_lines = len(code_lines)
    if effective_lines > MAX_EFFECTIVE_LINES:
        violations.append(
            Violation(
                path,
                effective_lines,
                "LINE_LIMIT",
                f"有效代码行数 {effective_lines} 超过上限 {MAX_EFFECTIVE_LINES}",
            )
        )

    if len(decls) > MAX_TYPES_PER_FILE:
        names = ", ".join(f"{k} {n}" for _, k, n in decls)
        violations.append(
            Violation(
                path,
                decls[MAX_TYPES_PER_FILE][0],
                "TYPE_COUNT",
                f"单文件数据结构数量 {len(decls)} 超过上限 {MAX_TYPES_PER_FILE} ({names})",
            )
        )

    for decl_line, kind, name in decls:
        err = check_type_has_chinese_doc(decl_line, blocks, attr_ranges, source_lines)
        if err is not None:
            violations.append(
                Violation(path, decl_line, "DOC_COMMENT", f"{kind} {name}: {err}")
            )

    for block in blocks:
        text_lines = extract_comment_text_lines(block)
        joined = "\n".join(text_lines)
        if not joined.strip():
            violations.append(
                Violation(path, block.start_line, "COMMENT_EMPTY", "注释块内容为空")
            )
            continue
        if not has_cjk(joined):
            violations.append(
                Violation(
                    path,
                    block.start_line,
                    "COMMENT_LANG",
                    "注释块必须包含简体中文说明",
                )
            )
        if not check_last_comment_punctuation(text_lines):
            violations.append(
                Violation(
                    path,
                    block.end_line,
                    "COMMENT_PERIOD",
                    "连续注释块最后一句禁止以句号（。/./。”）结尾",
                )
            )

    max_params = 0
    for fn_line, fn_name, param_count in fns:
        if param_count > max_params:
            max_params = param_count
        if param_count > MAX_FN_PARAMS:
            violations.append(
                Violation(
                    path,
                    fn_line,
                    "PARAM_COUNT",
                    f"函数 {fn_name} 参数数量 {param_count} 超过上限 {MAX_FN_PARAMS}",
                )
            )

    stats = {
        "effective_lines": effective_lines,
        "types": len(decls),
        "fns": len(fns),
        "max_params": max_params,
    }
    return violations, stats


def collect_targets(roots: List[str]) -> Tuple[List[Path], List[Path]]:
    dirs: List[Path] = []
    rs_files: List[Path] = []
    seen_dirs: set[Path] = set()
    seen_files: set[Path] = set()

    for raw in roots:
        p = Path(raw)
        if not p.exists():
            continue
        if p.is_file():
            if p.suffix == ".rs":
                rp = p.resolve()
                if rp not in seen_files:
                    seen_files.add(rp)
                    rs_files.append(p)
            continue

        for current_root, subdirs, files in os.walk(p):
            subdirs[:] = sorted(d for d in subdirs if d not in SKIP_DIRS)
            root_path = Path(current_root)
            resolved_dir = root_path.resolve()
            if resolved_dir not in seen_dirs:
                seen_dirs.add(resolved_dir)
                dirs.append(root_path)
            for fname in sorted(files):
                if fname.endswith(".rs"):
                    fp = root_path / fname
                    rfp = fp.resolve()
                    if rfp not in seen_files:
                        seen_files.add(rfp)
                        rs_files.append(fp)

    return dirs, sorted(rs_files)


def main(argv: List[str]) -> int:
    roots = argv[1:] if len(argv) > 1 else ["crates"]
    dirs, rs_files = collect_targets(roots)
    if not rs_files:
        print(f"No .rs files found under: {', '.join(roots)}", file=sys.stderr)
        return 1

    violations: List[Violation] = []
    max_dir_files = 0
    for d in dirs:
        direct_files = [entry for entry in os.scandir(d) if entry.is_file()]
        count = len(direct_files)
        if count > max_dir_files:
            max_dir_files = count
        if count > MAX_DIR_FILES:
            violations.append(
                Violation(
                    d,
                    1,
                    "DIR_FILES",
                    f"目录直接文件数 {count} 超过上限 {MAX_DIR_FILES}",
                )
            )

    max_lines = 0
    max_types = 0
    max_params = 0
    for fp in rs_files:
        file_violations, stats = check_file(fp)
        violations.extend(file_violations)
        max_lines = max(max_lines, stats["effective_lines"])
        max_types = max(max_types, stats["types"])
        max_params = max(max_params, stats["max_params"])

    if violations:
        for v in violations:
            print(f"{v.path}:{v.line}: [{v.rule}] {v.detail}")
        print(
            f"\nFAILED: {len(violations)} violation(s) across {len(rs_files)} file(s).",
            file=sys.stderr,
        )
        return 1

    print(
        f"OK: checked {len(rs_files)} Rust file(s) across {len(dirs)} dir(s) "
        f"(max_lines={max_lines}/{MAX_EFFECTIVE_LINES}, "
        f"max_types={max_types}/{MAX_TYPES_PER_FILE}, "
        f"max_params={max_params}/{MAX_FN_PARAMS}, "
        f"max_dir_files={max_dir_files}/{MAX_DIR_FILES})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
