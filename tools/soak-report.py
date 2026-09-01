#!/usr/bin/env python3
"""Summarize Crossover soak logs (see docs/SOAK.md).

Reads one or more `crossover run` log files and reports, per file:
transactions by result, latency percentiles, retry and contention
counts, session churn, and every error-level line.

Deliberately dependency-free and forgiving: soak logs come off real
machines and may be truncated, interleaved, or ANSI-coloured. Lines that
do not parse are counted, not fatal — a report that refuses to run
because of one odd line would be worse than useless at 2am.
"""

from __future__ import annotations

import re
import sys
from collections import Counter
from pathlib import Path

# tracing's default format, with ANSI escapes stripped first.
ANSI = re.compile(r"\x1b\[[0-9;]*m")
FIELD = re.compile(r"(\w+)=(\"[^\"]*\"|\S+)")


def parse_fields(line: str) -> dict[str, str]:
    """Extract key=value fields from a tracing line."""
    return {k: v.strip('"') for k, v in FIELD.findall(line)}


def percentile(values: list[float], pct: float) -> float:
    """Nearest-rank percentile; returns 0.0 for an empty sample."""
    if not values:
        return 0.0
    ordered = sorted(values)
    index = min(len(ordered) - 1, int(round(pct / 100.0 * len(ordered) + 0.5)) - 1)
    return ordered[max(0, index)]


def summarize(path: Path) -> None:
    results: Counter[str] = Counter()
    latencies: list[float] = []
    installs = 0
    retries = 0
    parked = 0
    contention = 0
    established = 0
    disconnects = 0
    pairings = 0
    errors: list[str] = []
    unparsed = 0

    try:
        raw = path.read_text(encoding="utf-8", errors="replace")
    except OSError as exc:
        print(f"{path}: cannot read ({exc})")
        return

    for line in raw.splitlines():
        line = ANSI.sub("", line).strip()
        if not line:
            continue
        fields = parse_fields(line)

        if "clipboard transaction closed" in line:
            results[fields.get("result", "unknown")] += 1
            if (ms := fields.get("latency_ms")) is not None:
                try:
                    latencies.append(float(ms))
                except ValueError:
                    unparsed += 1
        elif "clipboard item installed" in line:
            installs += 1
        elif "retry scheduled" in line:
            retries += 1
        # Checked before the generic busy line it would otherwise match:
        # an install that had to park is a different, rarer event than a
        # contended attempt (ADR 0005, addendum 2026-09-01).
        elif "parking the install" in line:
            parked += 1
        elif "busy" in line.lower() and "clipboard" in line.lower():
            contention += 1
        elif "session established" in line:
            established += 1
        elif "session ended" in line or "session establishment failed" in line:
            disconnects += 1
        elif "pairing succeeded" in line:
            pairings += 1

        if " ERROR " in line:
            errors.append(line)

    total = sum(results.values())
    print(f"\n=== {path.name} ===")
    print(f"  sessions established : {established}")
    print(f"  session ends/failures: {disconnects}")
    if pairings:
        print(f"  pairings             : {pairings}")
    print(f"  items installed here : {installs}")
    print(f"  transactions closed  : {total}")
    for result, count in results.most_common():
        share = f"{100.0 * count / total:.1f}%" if total else "-"
        print(f"      {result:<22} {count:>6}  {share}")
    if latencies:
        print(
            "  latency ms           : "
            f"p50={percentile(latencies, 50):.0f} "
            f"p95={percentile(latencies, 95):.0f} "
            f"max={max(latencies):.0f}"
        )
    print(f"  write retries        : {retries}")
    if parked:
        print(f"  installs parked      : {parked}")
    print(f"  contention events    : {contention}")
    if unparsed:
        print(f"  unparsed values      : {unparsed}")

    # Interpretation, stated rather than left to the reader's memory.
    notes: list[str] = []
    if results.get("clipboard_unavailable"):
        notes.append(
            f"{results['clipboard_unavailable']} item(s) never reached the "
            "destination clipboard — expected only if contention was staged"
        )
    if results.get("content_rejected"):
        notes.append(
            f"{results['content_rejected']} item(s) were rejected by the "
            "destination — investigate, this should not happen between "
            "conforming peers"
        )
    if results.get("superseded"):
        notes.append(
            f"{results['superseded']} item(s) lost a conflict race — normal "
            "only if both machines copied at once"
        )
    if parked:
        notes.append(
            f"{parked} install(s) outlived the fast retry budget and were parked — "
            "not a failure in itself; read it against clipboard_unavailable above"
        )
    if disconnects and not established:
        notes.append("sessions failed without ever establishing — check the firewall rule")
    if notes:
        print("  notes:")
        for note in notes:
            print(f"      - {note}")
    if errors:
        print(f"  ERROR lines ({len(errors)}):")
        for line in errors[:10]:
            print(f"      {line[:160]}")
        if len(errors) > 10:
            print(f"      ... and {len(errors) - 10} more")


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        print("usage: soak-report.py <log> [<log> ...]")
        return 2
    for arg in sys.argv[1:]:
        summarize(Path(arg))
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
