"""Render benchmark results (from `examples/bench.rs --json`) as PNG charts.

Usage::

    cargo run --release -p graphydra-core --example bench -- 100000 500000 \
        --json docs/benchmarks/results-100k-500k.json
    python scripts/plot_bench.py docs/benchmarks/results-100k-500k.json

Writes ``<stem>-latency-{light,dark}.png`` and ``<stem>-phases-{light,dark}.png``
next to the JSON file, plus ``<stem>.md`` with the same numbers as a table.
Colors are a validated, colorblind-safe categorical palette (see docs/performance.md).
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import FuncFormatter, LogLocator  # noqa: E402

THEMES = {
    "light": {
        "surface": "#fcfcfb",
        "ink": "#0b0b0b",
        "ink2": "#52514e",
        "muted": "#898781",
        "grid": "#e1e0d9",
        "axis": "#c3c2b7",
        "series": ["#2a78d6", "#eb6834", "#1baf7a"],
    },
    "dark": {
        "surface": "#1a1a19",
        "ink": "#ffffff",
        "ink2": "#c3c2b7",
        "muted": "#898781",
        "grid": "#2c2c2a",
        "axis": "#383835",
        "series": ["#3987e5", "#d95926", "#199e70"],
    },
}
FONT = ["Segoe UI", "Helvetica Neue", "Arial", "DejaVu Sans"]


def fmt_ms(v: float) -> str:
    if v >= 100:
        return f"{v:,.0f} ms"
    if v >= 1:
        return f"{v:.1f} ms"
    if v >= 0.1:
        return f"{v:.2f} ms"
    return f"{v * 1000:.0f} µs"


def style_axes(ax: plt.Axes, t: dict[str, object]) -> None:
    ax.set_facecolor(t["surface"])
    for side in ("top", "right", "left"):
        ax.spines[side].set_visible(False)
    ax.spines["bottom"].set_color(t["axis"])
    ax.tick_params(colors=t["muted"], labelsize=9, length=0)
    ax.xaxis.grid(True, color=t["grid"], linewidth=0.8)
    ax.set_axisbelow(True)


def title(fig: plt.Figure, t: dict[str, object], text: str, sub: str) -> float:
    """Draw title + subtitle at fixed inch offsets; returns the plot's top edge (figure fraction)."""
    h = fig.get_figheight()
    fig.text(0.012, 1 - 0.14 / h, text, ha="left", va="top", fontsize=13, fontweight="bold", color=t["ink"])
    fig.text(0.012, 1 - 0.46 / h, sub, ha="left", va="top", fontsize=9.5, color=t["ink2"])
    return 1 - 0.95 / h


def latency_chart(data: dict, theme: str, out: Path) -> None:
    t = THEMES[theme]
    queries = data["queries"]
    names = [q["query"] for q in queries][::-1]
    stats = [("median", "median_ms"), ("p95", "p95_ms"), ("p99", "p99_ms")]
    n = len(names)
    fig, ax = plt.subplots(figsize=(10, 0.52 * n + 1.9), dpi=160)
    fig.patch.set_facecolor(t["surface"])
    style_axes(ax, t)
    bar_h, gap = 0.22, 0.03  # thin bars with a surface gap between them
    for k, (label, key) in enumerate(stats):
        values = [q[key] for q in queries][::-1]
        offsets = [i + (1 - k) * (bar_h + gap) for i in range(n)]
        ax.barh(offsets, values, height=bar_h, color=t["series"][k], label=label, zorder=3)
    medians = [q["median_ms"] for q in queries][::-1]
    p99s = [q["p99_ms"] for q in queries][::-1]
    for i, (m, p) in enumerate(zip(medians, p99s)):
        # Direct label: the median, placed past the longest bar of the group.
        ax.text(p * 1.12, i, f"median {fmt_ms(m)}", va="center", fontsize=8.5, color=t["ink2"])
    ax.set_xscale("log")
    ax.xaxis.set_major_locator(LogLocator(base=10))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: fmt_ms(v)))
    lo = min(medians) / 2
    ax.set_xlim(lo, max(p99s) * 8)
    ax.set_yticks(range(n))
    ax.set_yticklabels(names, color=t["ink"], fontsize=9.5)
    ax.set_xlabel("latency per query (log scale, lower is better)", color=t["muted"], fontsize=9)
    leg = ax.legend(loc="lower right", frameon=False, fontsize=9, ncols=3, bbox_to_anchor=(1.0, 1.0))
    for txt in leg.get_texts():
        txt.set_color(t["ink2"])
    top = title(
        fig,
        t,
        "GrapHydra query latency",
        f"{data['nodes']:,} nodes · {data['edges']:,} edges · warm cache · {data['os']}/{data['arch']} · v{data['version']}",
    )
    fig.tight_layout(rect=(0, 0, 1, top))
    fig.savefig(out, facecolor=t["surface"])
    plt.close(fig)


def phases_chart(data: dict, theme: str, out: Path) -> None:
    t = THEMES[theme]
    phases = data["phases"][::-1]
    names = [p["phase"] for p in phases]
    secs = [p["seconds"] for p in phases]
    fig, ax = plt.subplots(figsize=(10, 0.5 * len(names) + 1.8), dpi=160)
    fig.patch.set_facecolor(t["surface"])
    style_axes(ax, t)
    ax.barh(range(len(names)), secs, height=0.5, color=t["series"][0], zorder=3)
    for i, s in enumerate(secs):
        ax.text(s + max(secs) * 0.015, i, f"{s:.2f} s", va="center", fontsize=9, color=t["ink2"])
    ax.set_xlim(0, max(secs) * 1.18)
    ax.set_yticks(range(len(names)))
    ax.set_yticklabels(names, color=t["ink"], fontsize=9.5)
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g} s"))
    ax.set_xlabel("wall-clock time (lower is better)", color=t["muted"], fontsize=9)
    mem = f" · peak RSS {data['peak_rss_mib']:.0f} MiB" if data.get("peak_rss_mib") else ""
    top = title(
        fig,
        t,
        "GrapHydra ingest, checkpoint and cold open",
        f"{data['nodes']:,} nodes · {data['edges']:,} edges · fsync on · snapshot {data['snapshot_mib']:.1f} MiB{mem}",
    )
    fig.tight_layout(rect=(0, 0, 1, top))
    fig.savefig(out, facecolor=t["surface"])
    plt.close(fig)


def table(data: dict) -> str:
    lines = [
        f"### {data['nodes']:,} nodes, {data['edges']:,} edges ({data['os']}/{data['arch']}, v{data['version']})",
        "",
        "| phase | seconds |",
        "|---|---:|",
    ]
    lines += [f"| {p['phase']} | {p['seconds']:.2f} |" for p in data["phases"]]
    lines += ["", "| query | median ms | p95 ms | p99 ms | rows |", "|---|---:|---:|---:|---:|"]
    lines += [
        f"| {q['query']} | {q['median_ms']:.3f} | {q['p95_ms']:.3f} | {q['p99_ms']:.3f} | {q['rows']:.1f} |"
        for q in data["queries"]
    ]
    if data.get("peak_rss_mib"):
        lines += ["", f"Peak resident memory: {data['peak_rss_mib']:.0f} MiB; snapshot size {data['snapshot_mib']:.1f} MiB."]
    return "\n".join(lines) + "\n"


def main(paths: list[str]) -> None:
    from matplotlib import font_manager

    available = {f.name for f in font_manager.fontManager.ttflist}
    plt.rcParams["font.family"] = [f for f in FONT if f in available] or ["DejaVu Sans"]
    for p in paths:
        src = Path(p)
        data = json.loads(src.read_text(encoding="utf-8"))
        for theme in THEMES:
            latency_chart(data, theme, src.with_name(f"{src.stem}-latency-{theme}.png"))
            phases_chart(data, theme, src.with_name(f"{src.stem}-phases-{theme}.png"))
        src.with_suffix(".md").write_text(table(data), encoding="utf-8")
        print(f"rendered charts for {src}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1:])
