#!/usr/bin/env python3
"""
Polymarket Data Harvester: 抓取 @late-to-tomorrow 全量历史成交记录

请使用虚拟环境运行：
  cd scripts && python3 -m venv .venv && source .venv/bin/activate
  pip install -r requirements.txt && python late_to_tomorrow_harvester.py

- Step1: profiles API 解析 proxyWallet
- Step2: 分页 trades (limit=100, delay=0.2s)，实时追加 CSV
- Step3: 字段 成交时间(UTC)、市场Slug、Side、Price、Size、Total USD
- Step4: 结束后输出 总交易额、平均持仓时长分布、最频繁市场 Top10
"""

import csv
import os
import sys
import time
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

import requests

BASE = "https://gamma-api.polymarket.com"
# 用户交易记录使用 Polymarket Data API（Gamma 的 /trades 会 404）
TRADES_BASE = "https://data-api.polymarket.com"
USERNAME = "late-to-tomorrow"
OUTPUT_CSV = Path(__file__).resolve().parent / "late_to_tomorrow_full_trades.csv"
DELAY_SEC = 0.2
PAGE_SIZE = 100

# 尽量模拟浏览器，部分 Gamma 接口对 Referer 有要求
HEADERS = {
    "User-Agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "Accept": "application/json",
    "Referer": "https://polymarket.com/",
    "Origin": "https://polymarket.com",
}


def get_proxy_wallet() -> str:
    """Step 1: 通过 username 获取 proxyWallet，或从环境变量/命令行读取"""
    # 优先使用环境变量或命令行参数（profiles 接口可能返回 401）
    wallet = os.environ.get("PROXY_WALLET", "").strip()
    if not wallet and len(sys.argv) > 1:
        wallet = sys.argv[1].strip()
    if wallet:
        return wallet
    url = f"{BASE}/profiles"
    params = {"username": USERNAME}
    r = requests.get(url, params=params, headers=HEADERS, timeout=30)
    r.raise_for_status()
    data = r.json()
    if isinstance(data, list) and data:
        profile = data[0]
    elif isinstance(data, dict):
        profile = data
    else:
        raise ValueError(f"No profile found for {USERNAME}")
    wallet = profile.get("proxyWallet") or profile.get("proxy_wallet") or profile.get("address")
    if not wallet:
        raise ValueError(f"proxyWallet not found in profile: {profile}")
    return wallet


def fetch_trades_page(wallet: str, offset: int) -> list[dict]:
    """Step 2: 分页请求 trades（Data API 用 user 参数）。offset 超范围时 API 可能 400，视为无更多数据返回 []"""
    url = f"{TRADES_BASE}/trades"
    params = {"user": wallet, "limit": PAGE_SIZE, "offset": offset}
    try:
        r = requests.get(url, params=params, headers=HEADERS, timeout=30)
        r.raise_for_status()
    except requests.exceptions.HTTPError as e:
        if e.response is not None and e.response.status_code in (400, 404):
            return []
        raise
    data = r.json()
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        return data.get("trades", data.get("data", []))
    return []


def extract_row(t: dict) -> dict:
    """提取单条记录的字段（兼容 Data API：slug/eventSlug/size/price/timestamp）"""
    size = float(t.get("size", t.get("amount", 0)) or 0)
    price = float(t.get("price", t.get("outcomePrice", 0)) or 0)
    total_usd = size * price if (size and price) else float(t.get("totalCost", t.get("value", 0)) or 0)
    ts = t.get("timestamp") or t.get("createdAt") or t.get("time")
    if isinstance(ts, (int, float)):
        ts_sec = ts / 1000 if ts > 1e12 else ts
        dt = datetime.fromtimestamp(ts_sec, tz=timezone.utc)
        ts_str = dt.strftime("%Y-%m-%d %H:%M:%S UTC")
    else:
        ts_str = str(ts) if ts else ""
    market = (
        t.get("slug")
        or t.get("eventSlug")
        or t.get("marketSlug")
        or t.get("market_slug")
        or (t.get("market") or "")
    )
    if isinstance(market, dict):
        market = market.get("slug", market.get("question", "")) or ""
    side = t.get("side", t.get("outcome", t.get("type", "")))
    return {
        "timestamp_utc": ts_str,
        "market_slug": market,
        "side": side,
        "price": round(price, 6),
        "size": round(size, 6),
        "total_usd": round(total_usd, 4),
    }


def main():
    if os.environ.get("PROXY_WALLET") or len(sys.argv) > 1:
        print("[1/4] Using proxyWallet from PROXY_WALLET or argv...")
    else:
        print(f"[1/4] Resolving proxyWallet for @{USERNAME}...")
    try:
        wallet = get_proxy_wallet()
    except requests.exceptions.HTTPError as e:
        if e.response is not None and e.response.status_code == 401:
            print(
                "\n      Gamma /profiles 返回 401，无法通过用户名解析钱包。\n"
                "      请先查到 @late-to-tomorrow 的 proxy 钱包地址，再任选一种方式运行：\n\n"
                "        export PROXY_WALLET=0x...\n"
                "        python late_to_tomorrow_harvester.py\n\n"
                "        或:  python late_to_tomorrow_harvester.py 0x...\n",
                file=sys.stderr,
            )
            sys.exit(1)
        raise
    print(f"      proxyWallet: {wallet}")

    print(f"[2/4] Fetching trades (limit={PAGE_SIZE}, delay={DELAY_SEC}s)...")
    fieldnames = ["timestamp_utc", "market_slug", "side", "price", "size", "total_usd"]
    total_rows = 0
    total_usd = 0.0
    buys_by_market: dict[str, list[tuple[str, float]]] = defaultdict(list)
    sells_by_market: dict[str, list[tuple[str, float]]] = defaultdict(list)
    market_counts: dict[str, int] = defaultdict(int)

    with open(OUTPUT_CSV, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=fieldnames)
        w.writeheader()

        offset = 0
        while True:
            trades = fetch_trades_page(wallet, offset)
            if not trades:
                break
            for t in trades:
                row = extract_row(t)
                w.writerow(row)
                total_rows += 1
                total_usd += row["total_usd"]
                slug = row["market_slug"] or "(unknown)"
                market_counts[slug] += 1
                ts = row["timestamp_utc"]
                if str(row["side"]).lower() in ("buy", "yes", "0"):
                    buys_by_market[slug].append((ts, row["size"]))
                else:
                    sells_by_market[slug].append((ts, row["size"]))
            print(f"      offset={offset} -> +{len(trades)} rows (total {total_rows})")
            offset += len(trades)
            if len(trades) < PAGE_SIZE:
                break
            time.sleep(DELAY_SEC)

    print(f"\n[3/4] Saved to {OUTPUT_CSV} ({total_rows} rows)")

    # Step 4: 统计
    print("\n[4/4] === 统计结果 ===")
    print(f"  总交易笔数: {total_rows}")
    print(f"  总交易额 (Total USD): ${total_usd:,.2f}")

    # 平均持仓时长（简化：同一市场 buy/sell 配对，取时间差）
    hold_hours: list[float] = []
    for slug, buys in buys_by_market.items():
        sells = sells_by_market.get(slug, [])
        if not buys or not sells:
            continue
        try:
            buy_times = [datetime.strptime(b[0].replace(" UTC", ""), "%Y-%m-%d %H:%M:%S") for b in buys]
            sell_times = [datetime.strptime(s[0].replace(" UTC", ""), "%Y-%m-%d %H:%M:%S") for s in sells]
            buy_times.sort()
            sell_times.sort()
            for i, bt in enumerate(buy_times):
                for st in sell_times:
                    if st > bt:
                        hold_hours.append((st - bt).total_seconds() / 3600)
                        break
        except Exception:
            pass
    if hold_hours:
        avg_hold = sum(hold_hours) / len(hold_hours)
        print(f"  平均持仓时长: {avg_hold:.2f} 小时 (基于 {len(hold_hours)} 个配对)")
        # 分布
        bins = [(0, 1), (1, 6), (6, 24), (24, 168), (168, float("inf"))]
        labels = ["<1h", "1-6h", "6-24h", "1-7d", ">7d"]
        dist = [0] * len(bins)
        for h in hold_hours:
            for i, (lo, hi) in enumerate(bins):
                if lo <= h < hi:
                    dist[i] += 1
                    break
        print("  持仓时长分布:")
        for lbl, cnt in zip(labels, dist):
            pct = 100 * cnt / len(hold_hours) if hold_hours else 0
            print(f"    {lbl}: {cnt} ({pct:.1f}%)")
    else:
        print("  平均持仓时长: (无有效 buy/sell 配对)")

    # 最频繁交易的市场
    top_markets = sorted(market_counts.items(), key=lambda x: -x[1])[:10]
    print("\n  最频繁交易的市场 (Top 10):")
    for slug, cnt in top_markets:
        short = (slug[:50] + "…") if len(slug) > 50 else slug
        print(f"    {short}: {cnt} 笔")


if __name__ == "__main__":
    main()
