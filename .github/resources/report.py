#!/usr/bin/env python3
"""Turns measure.py's JSON into the Markdown comment on a pull request.

    report.py resources.json > comment.md

With ANTHROPIC_API_KEY set, it counts the tokens agents read with Claude's own
tokenizer; without it, it gives their size in bytes. The JSON comes from the pull
request's code, so only its numbers reach the comment, never its text.
"""

import json
import os
import re
import sys

MARKER = "<!-- kanzo-resources -->"
MODEL = "claude-opus-5-5"


class Tokens:
    """Counts tokens the way agents pay for them: what a request costs more with the text in it."""

    def __init__(self):
        from anthropic import Anthropic

        self.client = Anthropic()
        self.empty = self.count()

    def count(self, text=".", **extra):
        return self.client.messages.count_tokens(
            model=MODEL, messages=[{"role": "user", "content": text}], **extra
        ).input_tokens

    def text(self, text):
        return self.count(text or ".") - self.empty

    def tools(self, tools):
        defs = [{"name": t["name"], "description": t.get("description", ""), "input_schema": t["inputSchema"]} for t in tools]
        return self.count(tools=defs) - self.empty

    def system(self, text):
        return self.count(system=text) - self.empty if text else 0


def label(name):
    """A reply's name from the JSON, kept to plain characters so it can't add Markdown or mentions."""
    return re.sub(r"[^a-z0-9_ ()]", "", name.lower())[:40]


def agents_rows(build, tokens):
    """Tool list, instructions and replies, in tokens or in bytes."""
    if tokens:
        rows = {
            "Tool list, read every session": (tokens.tools(build["tools"]), "tokens"),
            "Instructions, read every session": (tokens.system(build["instructions"]), "tokens"),
        }
        for name, text in build["replies"].items():
            rows[f"Reply to `{label(name)}`"] = (tokens.text(text), "tokens")
        return rows
    rows = {
        "Tool list, read every session": (build["tools_bytes"], "bytes"),
        "Instructions, read every session": (len(build["instructions"].encode()), "bytes"),
    }
    for name, text in build["replies"].items():
        rows[f"Reply to `{label(name)}`"] = (len(text.encode()), "bytes")
    return rows


def size(n):
    n = abs(n)
    if n >= 1_000_000:
        return f"{n / 1_000_000:.2f} MB"
    if n >= 1_000:
        return f"{n / 1_000:.1f} KB"
    return f"{n:.0f} B"


def fmt(value, unit):
    if unit == "bytes":
        return size(value)
    if unit == "ms":
        return f"{value:.1f} ms"
    return f"{value:,.0f} {unit}" if unit else f"{value:,.0f}"


def change(base, head, unit, noise):
    """The change and whether it's more than noise: 1% for sizes and counts, 15% for times."""
    diff = head - base
    if diff == 0 or (unit == "ms" and abs(diff) < 0.5):
        return "same", False
    pct = diff / base * 100 if base else 100.0
    sign = "+" if diff > 0 else "−"
    text = f"{sign}{fmt(abs(diff), unit)} ({sign}{abs(pct):.1f}%)"
    real = abs(pct) >= noise
    return (f"**{text}**" if real else text), real


def main():
    with open(sys.argv[1]) as f:
        data = json.load(f)
    base, head = data["base"], data["head"]
    tokens = Tokens() if os.environ.get("ANTHROPIC_API_KEY") else None

    # (section, name, key, unit, noise in %)
    measured = [
        ("Program", "Binary", "binary_bytes", "bytes", 1),
        ("Program", "Crates in Cargo.lock", "crates", "", 0),
        ("Memory", "Idle board", "idle_rss_bytes", "bytes", 5),
        ("Memory", f"Board with {data['tasks']:,} tasks loaded", "loaded_rss_bytes", "bytes", 5),
        ("Memory", "Peak of the board", "peak_rss_bytes", "bytes", 5),
        ("Memory", "`kanzo mcp` after a session", "mcp_rss_bytes", "bytes", 5),
        ("Memory", "Threads when idle", "idle_threads", "", 0),
        ("Speed", "Start", "start_ms", "ms", 15),
        ("Speed", f"`/api/board` with {data['tasks']:,} tasks", "board_ms", "ms", 15),
        ("Storage", "Empty database", "empty_db_bytes", "bytes", 1),
        ("Storage", f"Database with {data['tasks']:,} tasks", "db_bytes", "bytes", 1),
        ("Storage", f"`/api/board` reply with {data['tasks']:,} tasks", "board_bytes", "bytes", 1),
        ("Storage", "Board page (HTML, JS, logo)", "page_bytes", "bytes", 1),
    ]
    sections = {"Agents": []}
    for section, name, key, unit, noise in measured:
        if key in base and key in head:
            sections.setdefault(section, []).append((name, base[key], head[key], unit, noise))
    base_agents, head_agents = agents_rows(base, tokens), agents_rows(head, tokens)
    for name, (head_value, unit) in head_agents.items():
        base_value = base_agents.get(name, (0, unit))[0]
        sections["Agents"].append((name, base_value, head_value, unit, 1))

    lines, moved = [], {}
    for section, rows in sections.items():
        lines += [f"**{section}**", "", "| | main | this PR | change |", "| --- | ---: | ---: | ---: |"]
        for name, b, h, unit, noise in rows:
            text, real = change(b, h, unit, noise)
            lines.append(f"| {name} | {fmt(b, unit)} | {fmt(h, unit)} | {text} |")
            if real:
                short = re.sub(r" \(.*\)", "", name).split(", ")[0].replace("`", "")
                moved.setdefault(section, []).append(f"{short[0].lower()}{short[1:]} {text.strip('*').split(' (')[0]}")
        lines.append("")

    if moved:
        summary = "If merged, " + "; ".join(f"{s.lower()}: {', '.join(m)}" for s, m in moved.items()) + "."
    else:
        summary = "If merged, nothing moves by more than noise."
    note = (
        f"Tokens counted with `{MODEL}`'s tokenizer."
        if tokens
        else "Agents' text is in bytes: add an `ANTHROPIC_API_KEY` secret to count tokens."
    )
    print(MARKER)
    print("### Resources")
    print()
    print(summary)
    print()
    print("\n".join(lines))
    print(
        f"<sub>Measured on GitHub's ubuntu-26.04 runner, median of {data['rounds']} round{"s" if data["rounds"] != 1 else ""} with the builds taking turns. "
        f"Changes in bold are past noise: 1% for sizes, 5% for memory, 15% for times. {note}</sub>"
    )


if __name__ == "__main__":
    main()
