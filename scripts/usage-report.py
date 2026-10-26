#!/usr/bin/env python3
"""从 Lucia 用量台账生成逐轮用量表与会话成本汇总。

台账由 TUI 默认写入 `<sessions_dir>/<项目 ID>/usage/<会话 ID>.jsonl`：每次模型响应一行
`type=request`，扩展事件各一行 `type=extension`。本脚本只读台账与仓库内的 models.dev
快照，不访问网络，也不读取配置文件。

用法：
  python3 scripts/usage-report.py                       # 分析 ~/.lucia 下最近更新的台账
  python3 scripts/usage-report.py LEDGER.jsonl --rows 100:120
  python3 scripts/usage-report.py LEDGER.jsonl --protocol anthropic --csv rows.csv
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sys
from collections import Counter
from datetime import datetime
from pathlib import Path

# 输入侧单价比例（以普通输入为 1）：缓存写、缓存读。OpenAI 协议实测没有写入溢价。
PROTOCOL_RATIOS = {
    "openai": {"write": 1.0, "read": 0.1},
    "anthropic": {"write": 1.25, "read": 0.1},
}
# Anthropic 默认短期缓存 TTL；两次请求起点间隔超过它，前缀缓存大概率已过期。
CACHE_TTL_SECONDS = 300
# 原生上下文加载器发布的压缩事件及其在表格中的简称。
COMPACTION_EVENTS = {
    "context.micro_compaction.completed": "微压缩",
    "context.compaction.completed": "完整摘要",
    "context.compaction.skipped": "压缩跳过",
}
MODELS_DEV_SNAPSHOT = Path(__file__).resolve().parent.parent / "crates/agent-tui/assets/models-dev.json"
MODELS_DEV_PROVIDERS = ("openai", "anthropic", "deepseek", "google")


def find_latest_ledger() -> Path:
    """在 Lucia Home 的会话根目录下查找最近更新的台账。"""
    home = Path(os.environ.get("LUCIA_HOME") or Path.home() / ".lucia")
    candidates = list(home.glob("*/*/usage/*.jsonl"))
    if not candidates:
        sys.exit(f"在 {home} 下没有找到用量台账；请显式传入台账路径")
    return max(candidates, key=lambda path: path.stat().st_mtime)


def load_ledger(path: Path) -> tuple[list[dict], dict[tuple[str, int], list[dict]]]:
    """读取台账，返回按时间排列的请求行，以及按 (run_id, step) 归组的扩展事件行。"""
    requests: list[dict] = []
    extensions: dict[tuple[str, int], list[dict]] = {}
    with path.open(encoding="utf-8") as file:
        for number, line in enumerate(file, start=1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as error:
                sys.exit(f"{path}:{number} 不是合法 JSON：{error}")
            if row.get("type") == "request":
                requests.append(row)
            elif row.get("type") == "extension":
                extensions.setdefault((row.get("run_id"), row.get("step")), []).append(row)
    requests.sort(key=lambda row: row.get("ts_ms") or row.get("response_ts_ms") or 0)
    return requests, extensions


def usage_value(row: dict, key: str) -> int | None:
    """读取一项用量；服务商未回报时为 None。"""
    usage = row.get("usage") or {}
    value = usage.get(key)
    return value if isinstance(value, int) else None


def prompt_total(row: dict) -> int:
    """本次请求实际送达模型的输入总量：未缓存输入 + 缓存读 + 缓存写。"""
    return sum(usage_value(row, key) or 0 for key in ("input_tokens", "cache_read_tokens", "cache_write_tokens"))


def describe_compaction(events: list[dict]) -> str:
    """把同一步的原生压缩事件概括成一句；没有压缩时返回空串。"""
    parts = []
    for event in events:
        label = COMPACTION_EVENTS.get(event.get("name"))
        if label is None:
            continue
        data = event.get("data") or {}
        before, after = data.get("estimated_tokens_before"), data.get("estimated_tokens_after")
        detail = f"{label} {format_number(before)}→{format_number(after)}" if before is not None else label
        if data.get("cleared_tool_results"):
            detail += f" 清空{data['cleared_tool_results']}条"
        if data.get("summarized_messages"):
            detail += f" 摘要{data['summarized_messages']}条"
        parts.append(detail)
    return "；".join(parts)


def describe_prefix(prefix: dict | None) -> str:
    """概括本次请求相对上一次请求的前缀变化。"""
    if not prefix:
        return "无指纹"
    if prefix.get("previous_messages") is None:
        return "基线"
    flags = "".join(
        mark for mark, key in (("S", "system_changed"), ("T", "tools_changed"), ("P", "settings_changed")) if prefix.get(key)
    )
    if prefix.get("rewritten"):
        role = prefix.get("changed_role") or prefix.get("previous_role") or "?"
        text = f"改写@{prefix['stable_messages']}/{prefix['previous_messages']}({role})"
    else:
        text = f"追加+{prefix['messages'] - prefix['previous_messages']}"
    return f"{text} {flags}".strip()


def format_number(value) -> str:
    """千位分组；缺失值显示为 -。"""
    return "-" if value is None else f"{value:,}"


def load_prices(model: str | None) -> dict | None:
    """在 models.dev 快照中查找模型的每百万 token 官方价格；找不到时返回 None。"""
    if not model or not MODELS_DEV_SNAPSHOT.exists():
        return None
    snapshot = json.loads(MODELS_DEV_SNAPSHOT.read_text(encoding="utf-8"))
    for provider in MODELS_DEV_PROVIDERS:
        cost = snapshot.get(provider, {}).get("models", {}).get(model, {}).get("cost")
        if cost:
            return {"provider": provider, **cost}
    return None


def request_usd(row: dict, prices: dict) -> float:
    """按 models.dev 价格估算单次请求美元成本；输入总量超过分档阈值时使用分档价格。"""
    tier_prices = prices
    for tier in prices.get("tiers") or []:
        size = (tier.get("tier") or {}).get("size")
        if size is not None and prompt_total(row) > size:
            tier_prices = tier
    rates = {
        "input_tokens": tier_prices.get("input", 0),
        "cache_read_tokens": tier_prices.get("cache_read", 0),
        "cache_write_tokens": tier_prices.get("cache_write", 0),
        "output_tokens": tier_prices.get("output", 0),
    }
    return sum((usage_value(row, key) or 0) * rate for key, rate in rates.items()) / 1_000_000


def build_rows(requests: list[dict], extensions: dict) -> list[dict]:
    """把请求行整理成表格行：补充运行序号、与上一请求的间隔、清理与前缀说明。"""
    rows, run_index, previous_ts, runs = [], 0, None, {}
    for index, request in enumerate(requests, start=1):
        run_id = request.get("run_id")
        if run_id not in runs:
            run_index += 1
            runs[run_id] = run_index
        ts = request.get("ts_ms")
        gap = None if previous_ts is None or ts is None else (ts - previous_ts) / 1000
        previous_ts = ts if ts is not None else previous_ts
        read, total = usage_value(request, "cache_read_tokens"), prompt_total(request)
        rows.append({
            "序号": index,
            "时间": datetime.fromtimestamp(ts / 1000).strftime("%H:%M:%S") if ts else "-",
            "间隔秒": "-" if gap is None else f"{gap:.0f}" + ("*" if gap > CACHE_TTL_SECONDS else ""),
            "轮次": runs[run_id],
            "步": request.get("step"),
            "重试": request.get("retry_attempt") or 0,
            "估算上下文": (request.get("context") or {}).get("estimated_tokens"),
            "输入": usage_value(request, "input_tokens"),
            "缓存读": read,
            "缓存写": usage_value(request, "cache_write_tokens"),
            "输出": usage_value(request, "output_tokens"),
            "命中率": "-" if read is None or total == 0 else f"{read / total:.0%}",
            "清理": describe_compaction(extensions.get((run_id, request.get("step")), [])),
            "前缀": describe_prefix(request.get("prefix")),
        })
    return rows


def print_table(rows: list[dict]) -> None:
    """按列宽对齐打印表格；数字列右对齐。"""
    if not rows:
        print("台账中没有请求行")
        return
    numeric = {"估算上下文", "输入", "缓存读", "缓存写", "输出"}
    columns = list(rows[0])
    cells = [[format_number(row[column]) if column in numeric else str(row[column]) for column in columns] for row in rows]

    def width(text: str) -> int:
        return sum(2 if ord(char) > 0x2E7F else 1 for char in text)

    widths = [max(width(column), *(width(cell[i]) for cell in cells)) for i, column in enumerate(columns)]

    def render(values: list[str]) -> str:
        parts = []
        for i, value in enumerate(values):
            pad = " " * (widths[i] - width(value))
            parts.append(pad + value if columns[i] in numeric else value + pad)
        return "  ".join(parts).rstrip()

    print(render(columns))
    for cell in cells:
        print(render(cell))


def print_summary(requests: list[dict], rows: list[dict], protocol: str) -> None:
    """打印会话级汇总：用量合计、前缀改写分布、清理次数与成本估算。"""
    totals = {key: sum(usage_value(row, key) or 0 for row in requests) for key in ("input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens")}
    count = len(requests)
    prefixes = [row.get("prefix") or {} for row in requests]
    rewrites = Counter(prefix.get("changed_role") or prefix.get("previous_role") or "?" for prefix in prefixes if prefix.get("rewritten"))
    compactions = Counter(part.split(" ")[0] for row in rows for part in row["清理"].split("；") if part)
    idle = sum(1 for row in rows if row["间隔秒"].endswith("*"))
    prompt = totals["input_tokens"] + totals["cache_read_tokens"] + totals["cache_write_tokens"]
    ratios = PROTOCOL_RATIOS[protocol]
    relative = totals["input_tokens"] + ratios["write"] * totals["cache_write_tokens"] + ratios["read"] * totals["cache_read_tokens"]

    print()
    print(f"请求数 {count} · 用户轮次 {len({row.get('run_id') for row in requests})} · 空闲超过 {CACHE_TTL_SECONDS // 60} 分钟后的请求 {idle}")
    print("合计：" + " · ".join(f"{label} {format_number(totals[key])}" for label, key in (("未缓存输入", "input_tokens"), ("缓存读", "cache_read_tokens"), ("缓存写", "cache_write_tokens"), ("输出", "output_tokens"))))
    if count:
        print(f"平均每次请求：输入总量 {prompt / count:,.0f} · 缓存写 {totals['cache_write_tokens'] / count:,.0f} · 输出 {totals['output_tokens'] / count:,.0f}")
    if prompt:
        print(f"整体命中率（缓存读 ÷ 输入总量）：{totals['cache_read_tokens'] / prompt:.1%}")
    print("前缀改写：" + ("无" if not rewrites else " · ".join(f"{role} {times} 次" for role, times in rewrites.most_common())))
    print("system/工具/参数变化：" + " · ".join(f"{label} {sum(1 for prefix in prefixes if prefix.get(key))} 次" for label, key in (("system", "system_changed"), ("工具", "tools_changed"), ("参数", "settings_changed"))))
    print("清理：" + ("无" if not compactions else " · ".join(f"{name} {times} 次" for name, times in compactions.items())))
    print(f"输入侧成本（{protocol} 比例 输入 1 / 写 {ratios['write']} / 读 {ratios['read']}）：{relative:,.0f} 普通输入当量")
    prices = load_prices(next((row.get("model") for row in requests if row.get("model")), None))
    if prices:
        usd = sum(request_usd(row, prices) for row in requests)
        print(f"按 models.dev 官方价（{prices['provider']}）估算：${usd:,.4f}（含输出；网关实际计价可能不同）")


def main() -> None:
    parser = argparse.ArgumentParser(description="从 Lucia 用量台账生成逐轮用量表与会话成本汇总")
    parser.add_argument("ledger", nargs="?", type=Path, help="台账路径；缺省时取 ~/.lucia 下最近更新的台账")
    parser.add_argument("--rows", help="只打印指定序号区间，例如 100:120（含两端）")
    parser.add_argument("--protocol", choices=sorted(PROTOCOL_RATIOS), default="openai", help="输入侧成本比例，缺省 openai")
    parser.add_argument("--csv", type=Path, help="同时把全部表格行写入 CSV")
    args = parser.parse_args()

    path = args.ledger or find_latest_ledger()
    requests, extensions = load_ledger(path)
    rows = build_rows(requests, extensions)
    print(f"台账：{path}")
    shown = rows
    if args.rows:
        start, _, end = args.rows.partition(":")
        shown = [row for row in rows if int(start or 1) <= row["序号"] <= int(end or len(rows))]
    print_table(shown)
    print_summary(requests, rows, args.protocol)
    if args.csv:
        with args.csv.open("w", newline="", encoding="utf-8") as file:
            writer = csv.DictWriter(file, fieldnames=list(rows[0]) if rows else [])
            writer.writeheader()
            writer.writerows(rows)
        print(f"已写入 {args.csv}")


if __name__ == "__main__":
    main()
