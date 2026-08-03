#!/usr/bin/env python3
# Measures how much of a coding session survives into its Munshi summary.
#
# Munshi compresses a session heavily — on a 126-session sample the ratio was ~554:1 (0.86 GB of
# raw transcript against 1.59 MB of Markdown). That is only a problem if the *findable* facts are
# among the ones dropped, so this reports what survives, by category, over the whole local corpus.
#
# It joins on session id: summaries carry `session_id:` in frontmatter, and Claude Code transcripts
# are `<session-id>.jsonl`. Copilot sessions have no local Claude transcript and are skipped, so
# the matched count is normally well below the summary count.
#
# Everything is local and read-only. No model, no network, no vault.
#
# Two matching modes, and the gap between them is the point:
#
#   prefix      an error's leading 40 characters appear verbatim in the summary. This is a FLOOR,
#               and a misleadingly low one: those characters are nearly always boilerplate
#               ("error: could not compile", "FAIL src/lib/..."), not the distinctive part.
#   distinctive at least one non-boilerplate identifier from the error appears. This is a CEILING —
#               a token can land in the summary for unrelated reasons.
#
# Measured on the author's corpus: prefix said 0.0%, distinctive said 68.4%. Neither is the answer.
# Hand-judging a sample against full summaries put the useful number elsewhere again: of 24 sampled
# errors, 16 were transient dev-loop noise correctly dropped, 6 were significant, and 5 of those 6
# were represented in paraphrase. Treat this script as triage that tells you *where* to look, not
# as the verdict.
#
# Usage:
#   contrib/summary-retention.py                          # uses ~/munshi-summaries, ~/.claude/projects
#   contrib/summary-retention.py --summaries DIR --transcripts DIR
#   contrib/summary-retention.py --json report.json       # per-session detail for follow-up

import argparse
import glob
import json
import os
import re
import sys
from collections import defaultdict

# A failure, not merely an identifier containing "error". Case-sensitive on purpose: a
# case-insensitive `[A-Za-z_]*Error` matches `thiserror` in a Cargo.toml and every `HttpError::`
# in a match arm, which inflated the error population by ~23% in an earlier revision.
ERROR_RE = re.compile(
    r"(?:^|\n)\s*(?:"
    r"error(?:\[[A-Z]\d+\])?\s*:"
    r"|(?<![:.\w])(?:[A-Z][A-Za-z]*)?(?:Error|Exception)\s*:"
    r"|panicked at"
    r"|FAILED\b|FAIL\b"
    r"|no such file or directory"
    r")"
)

# Words that carry no search signal, so a match on them means nothing.
BOILERPLATE_RE = re.compile(
    r"^(error|fail|failed|could|not|compile|test|tests|due|to|previous|errors|rerun|pass|the|and"
    r"|of|in|on|at|is|no|such|file|directory|type|types|annotations|needed|mismatched|expected"
    r"|but|was|src|lib|crates|value|values|found|this|that|with|from)$",
    re.I,
)
TOKEN_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]{4,}")

# Truncating a tool result hides errors that print after a wall of output. 8 KiB was chosen
# against a corpus whose p90 result body is ~2.9 KB: it cost 5 of 238 error-bearing bodies at
# 4 KiB, and nothing measurable above that.
RESULT_SCAN_BYTES = 8192


def load_summaries(root):
    """session_id -> full summary text."""
    out = {}
    for path in glob.glob(os.path.join(root, "**", "*.md"), recursive=True):
        try:
            text = open(path, encoding="utf-8", errors="replace").read()
        except OSError:
            continue
        m = re.search(r'^session_id:\s*"([^"]+)"', text, re.M)
        if m:
            out[m.group(1)] = text
    return out


def content_items(entry):
    message = entry.get("message") or {}
    items = message.get("content")
    return items if isinstance(items, list) else []


def result_text(item):
    body = item.get("content")
    if isinstance(body, list):
        body = " ".join(x.get("text", "") for x in body if isinstance(x, dict))
    return body if isinstance(body, str) else ""


def extract(path):
    """High-signal facts from one transcript: what changed, what ran, what broke."""
    files_modified, commands, errors = set(), set(), set()
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                entry = json.loads(line)
            except (json.JSONDecodeError, ValueError):
                continue
            for item in content_items(entry):
                if not isinstance(item, dict):
                    continue
                kind = item.get("type")
                if kind == "tool_use":
                    name = item.get("name")
                    args = item.get("input")
                    if not isinstance(args, dict):
                        continue
                    # Files the session CHANGED. Files merely read are not facts about the work.
                    if name in ("Edit", "Write", "NotebookEdit"):
                        target = args.get("file_path") or args.get("notebook_path")
                        if target:
                            files_modified.add(os.path.basename(str(target)))
                    elif name == "Bash":
                        cmd = str(args.get("command", "")).strip()
                        if cmd:
                            commands.add(" ".join(cmd.split()[:2]))
                elif kind == "tool_result":
                    body = result_text(item)[:RESULT_SCAN_BYTES]
                    if not body:
                        continue
                    for m in ERROR_RE.finditer(body):
                        frag = re.sub(r"\s+", " ", body[m.start():m.start() + 70]).strip()
                        if len(frag) > 12:
                            errors.add(frag)
    return {"files_modified": files_modified, "commands": commands, "errors": errors}


def hit_prefix(fact, summary_lc):
    return fact.lower()[:40] in summary_lc


def hit_distinctive(fact, summary_lc):
    tokens = [t for t in TOKEN_RE.findall(fact) if not BOILERPLATE_RE.match(t)]
    return any(t.lower() in summary_lc for t in tokens)


def main():
    ap = argparse.ArgumentParser(
        description="Measure how much of a coding session survives into its Munshi summary.")
    ap.add_argument("--summaries", default=os.path.expanduser("~/munshi-summaries"),
                    help="Munshi output_directory (default: ~/munshi-summaries)")
    ap.add_argument("--transcripts", default=os.path.expanduser("~/.claude/projects"),
                    help="Claude Code projects root (default: ~/.claude/projects)")
    ap.add_argument("--json", metavar="PATH", help="write per-session detail here")
    args = ap.parse_args()

    summaries = load_summaries(args.summaries)
    if not summaries:
        print(f"no summaries with a session_id under {args.summaries}", file=sys.stderr)
        return 1
    print(f"summaries indexed: {len(summaries)}", file=sys.stderr)

    totals = defaultdict(lambda: {"prefix": 0, "distinctive": 0, "total": 0})
    detail, matched = [], 0
    for tx in glob.glob(os.path.join(args.transcripts, "*", "*.jsonl")):
        sid = os.path.basename(tx)[:-6]
        summary = summaries.get(sid)
        if summary is None:
            continue
        matched += 1
        summary_lc = summary.lower()
        facts = extract(tx)
        row = {"session": sid, "transcript_bytes": os.path.getsize(tx)}
        for cat, values in facts.items():
            p = sum(1 for f in values if hit_prefix(f, summary_lc))
            d = sum(1 for f in values if hit_distinctive(f, summary_lc))
            totals[cat]["prefix"] += p
            totals[cat]["distinctive"] += d
            totals[cat]["total"] += len(values)
            row[cat] = {"prefix": p, "distinctive": d, "total": len(values)}
        detail.append(row)

    print(f"sessions matched (transcript + summary): {matched}", file=sys.stderr)
    if not matched:
        print("nothing to report — check --transcripts", file=sys.stderr)
        return 1

    print()
    print(f"{'category':<18}{'total':>8}{'prefix':>10}{'distinctive':>14}")
    print("-" * 50)
    for cat in ("files_modified", "commands", "errors"):
        t = totals[cat]
        if not t["total"]:
            print(f"{cat:<18}{0:>8}{'—':>10}{'—':>14}")
            continue
        print(f"{cat:<18}{t['total']:>8}"
              f"{100.0 * t['prefix'] / t['total']:>9.1f}%"
              f"{100.0 * t['distinctive'] / t['total']:>13.1f}%")
    print()
    print("prefix is a floor (matches boilerplate); distinctive is a ceiling (incidental matches).")
    print("Judge a sample against full summaries before drawing a conclusion from either.")

    if args.json:
        with open(args.json, "w") as fh:
            json.dump({"totals": dict(totals), "sessions": detail}, fh, indent=1)
        print(f"\nper-session detail -> {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
