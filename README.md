# PolyStrike Engine

Institutional-grade high-frequency trading engine for [Polymarket](https://polymarket.com). Monitors Binance spot prices as an oracle, computes Black-Scholes binary option fair values, and executes bidirectional arbitrage (buy + sell) against the Polymarket CLOB when edge exceeds configurable thresholds.

**v1.0.0** — zero warnings, 10/10 unit tests, production-ready singleton architecture.

---

## Key Features

| Category | Detail |
|---|---|
| **Pricing Model** | Black-Scholes binary call pricing with cached `N(0,1)` distribution (zero allocation per tick) |
| **HFT State Machine** | `Empty → PendingBuy → Holding → PendingSell → Empty` with watchdog auto-unlock for stuck states |
| **Singleton Resources** | One `PolyClient` (auth + rate limiter) and one Binance WS connection shared across all tasks |
| **Risk Engine** | `Decimal`-precision budget, per-market exposure caps, hysteresis circuit breaker (freeze at 20% drawdown, thaw at 15%) |
| **Rate Limiter** | `Semaphore(8)` in `PolyClient` with 429 exponential backoff (3 retries) |
| **Graceful Shutdown** | SIGINT + SIGTERM → global `CancellationToken` → `cancel_all_orders` per token → final P&L snapshot |
| **Strong Reconciliation** | `sync_token_position` before/after each order to compute actual filled size; zero-fill = rollback |
| **Logging** | `tracing` with daily rolling file (`logs/engine.log`) + console; silent by default, speaks only on edge/fire |
| **SaaS API** | Axum HTTP API on port 3333 for dynamic strategy start/stop and event-based market discovery |

---

## Architecture

```
main.rs
 ├── AppConfig (from .env)
 ├── Arc<InventoryManager>     ── Decimal-precision in-memory ledger
 ├── Arc<RiskGuard>            ── Global budget + circuit breaker
 ├── Arc<StrategyRegistry>     ── token_id → CancellationToken
 ├── Option<Arc<PolyClient>>   ── Singleton authenticated CLOB client
 ├── watch::Receiver<BookTicker> ── Singleton Binance WS
 ├── CancellationToken         ── Global shutdown signal
 ├── Axum HTTP API (port 3333) ── Carries all singletons via ApiState
 ├── 30s Heartbeat task        ── "[System] Radar active. Monitoring N markets."
 └── wait_for_shutdown_signal  ── SIGINT / SIGTERM → cancel → 3s grace → snapshot
```

### Per-Market Task (`run_sniper_task`)

Each task receives injected singletons (no internal auth or WS creation):

```
run_sniper_task(token_id, strike_price, snipe_size, ...)
 ├── Poly WS (per-token orderbook stream)
 ├── Cloned Binance WS receiver
 ├── HFT State Machine (Empty/PendingBuy/Holding/PendingSell)
 ├── evaluate_and_act() on every tick
 │    ├── compute_edges() → Black-Scholes fair value → buy_edge / sell_edge
 │    ├── RiskGuard: is_frozen? check_market_exposure?
 │    └── tokio::spawn(try_fire(...)) → PendingBuy / PendingSell
 ├── fill_rx channel → state transitions + circuit breaker update
 ├── Watchdog: force-unlock Pending states after 8s
 └── On cancel: cancel_all_orders(token_id) → break
```

---

## Project Structure

```
src/
├── main.rs                    # Entry point, singleton init, shutdown
├── config.rs                  # AppConfig from .env
├── engine.rs                  # HFT state machine, evaluate_and_act, try_fire
├── api/
│   ├── server.rs              # Axum routes + ApiState
│   ├── models.rs              # StartStrategyReq, StopStrategyReq, StartEventRadarReq
│   └── strategy_registry.rs   # DashMap<token_id, CancellationToken>
├── execution/
│   └── poly_client.rs         # Authenticated CLOB client + Semaphore rate limiter
├── oracle/
│   ├── binance_ws.rs          # Binance @bookTicker stream with reconnection
│   └── poly_ws.rs             # Polymarket orderbook stream with reconnection
├── risk/
│   ├── guard.rs               # RiskGuard: budget, exposure, circuit breaker
│   ├── inventory.rs           # InventoryManager: positions, P&L (Decimal)
│   └── watchdog.rs            # Periodic risk breach scanner
├── strategy/
│   └── sniper.rs              # SniperStrategy + Black-Scholes pricing
├── discovery/
│   └── radar.rs               # Event market discovery via Polymarket Gamma API
└── models/
    ├── types.rs               # BookTicker, OrderSide
    └── btc_price.rs           # Global AtomicU64 for BTC mid price
```

---

## API Endpoints

| Method | Path | Body | Description |
|--------|------|------|-------------|
| `GET` | `/status` | — | Health check |
| `GET` | `/exposure` | — | All market exposures (yes/no qty, P&L) |
| `POST` | `/strategy/start` | `{ token_id, strike_price, snipe_size, expiry_timestamp, volatility }` | Start sniper for one market |
| `POST` | `/strategy/stop` | `{ token_id }` | Stop sniper for one market |
| `POST` | `/event/start` | `{ event_slug, volatility, snipe_size }` | Auto-discover all markets in an event and start snipers |

### Examples

```bash
# Start monitoring all markets in a Polymarket event
curl -X POST http://localhost:3333/event/start \
  -H 'Content-Type: application/json' \
  -d '{ "event_slug": "bitcoin-above-100k-2026", "volatility": 0.5, "snipe_size": 10.0 }'

# Start a single market
curl -X POST http://localhost:3333/strategy/start \
  -H 'Content-Type: application/json' \
  -d '{
    "token_id": "112493481455",
    "strike_price": 65000.0,
    "snipe_size": 10.0,
    "expiry_timestamp": 1741910400,
    "volatility": 0.5
  }'

# Stop a market
curl -X POST http://localhost:3333/strategy/stop \
  -H 'Content-Type: application/json' \
  -d '{ "token_id": "112493481455" }'

# Check status and exposure
curl http://localhost:3333/status
curl http://localhost:3333/exposure
```

---

## Configuration (.env)

```env
# Binance spot symbol (oracle)
BINANCE_SYMBOL=btcusdt

# Polygon chain ID (137 = mainnet)
CHAIN_ID=137

# CLOB private key (hex, for EIP-712 signing)
PK=0xYOUR_PRIVATE_KEY_HERE

# Optional: proxy wallet address
FUNDER_ADDRESS=0xYOUR_FUNDER_ADDRESS_HERE

# Risk parameters
MAX_BUDGET=5.0                    # Global max spend (USDC)
MAX_EXPOSURE_PER_MARKET=0.0       # Per-market cap (0 = unlimited)
RECONCILIATION_BUFFER_SECONDS=8.0

# Trading mode (SAFETY FIRST)
#   unset / true / 1  → DRY RUN (no real orders)
#   false / 0          → LIVE TRADING
DRY_RUN=true
```

### DRY_RUN Semantics

- **Default** (`DRY_RUN` unset): `dry_run = true` — paper trading, no real orders
- **Live trading**: must explicitly set `DRY_RUN=false` or `DRY_RUN=0`
- In dry run mode, `PolyClient` is **not initialized** (no auth network calls)

---

## Quick Start

```bash
# 1. Clone and configure
git clone https://github.com/liu-kaining/PolyStrikeEngine.git
cd PolyStrikeEngine
cp .env.example .env   # edit with your keys

# 2. Run tests
cargo test

# 3. Start engine (dry run by default)
cargo run

# 4. In another terminal, start monitoring an event
curl -X POST http://localhost:3333/event/start \
  -H 'Content-Type: application/json' \
  -d '{ "event_slug": "your-event-slug", "volatility": 0.5, "snipe_size": 10.0 }'
```

The engine will:
- Start the HTTP API on `http://0.0.0.0:3333`
- Connect one Binance WS stream (shared across all tasks)
- Wait for strategy start commands
- Monitor markets silently; log only when edge approaches fire threshold
- Exit gracefully on `Ctrl+C` or `SIGTERM`, cancelling all open orders first

---

## Risk Controls

| Control | Mechanism |
|---------|-----------|
| **Global Budget** | `RiskGuard::reserve_budget` pre-deducts; `release_budget` on failure; `refund_budget_on_sell` recycles capital |
| **Per-Market Exposure** | `check_market_exposure` rejects BUY if projected position exceeds cap |
| **Circuit Breaker** | Hysteresis: freezes all BUY at 20% drawdown, thaws at 15% recovery |
| **Pending Watchdog** | Force-unlocks `PendingBuy`/`PendingSell` after 8 seconds if no fill callback |
| **Order Timeout** | `try_fire` wraps execution in `tokio::time::timeout(5s)` |
| **Strong Reconciliation** | Compares `sync_token_position` before/after order; zero actual fill = rollback |
| **Rate Limiter** | `Semaphore(8)` + 429 exponential backoff in `PolyClient` |
| **Decimal Precision** | All monetary calculations use `rust_decimal::Decimal`; ToPrimitive for hot-path f64 conversion |

---

## Performance Optimizations

- **Singleton architecture**: one `PolyClient` and one Binance WS for 20+ concurrent market tasks
- **Zero-allocation hot path**: `ToPrimitive::to_f64()` instead of `Decimal::to_string().parse()`
- **Cached Normal distribution**: `OnceLock<Normal>` — Black-Scholes CDF computed without heap allocation
- **Monotonic time**: `std::time::Instant` in inventory (NTP-drift immune)
- **Lock-free reads**: `DashMap` for inventory and strategy registry
- **Non-blocking logging**: `tracing-appender` with `non_blocking` writer

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `axum` | HTTP API framework |
| `tokio-tungstenite` | WebSocket streams (Binance) |
| `polymarket-client-sdk` | CLOB + Data API (auth, orders, positions) |
| `rust_decimal` | Exact monetary arithmetic |
| `dashmap` | Lock-free concurrent HashMap |
| `statrs` | Normal CDF for Black-Scholes |
| `chrono` | Time-to-expiry calculation |
| `tracing` / `tracing-subscriber` / `tracing-appender` | Structured logging with file rotation |
| `tokio-util` | `CancellationToken` for graceful shutdown |
| `reqwest` | HTTP client for market discovery |
| `alloy` | Ethereum signer (EIP-712) |

---

## 中文说明

PolyStrike Engine 是面向 Polymarket 的机构级高频交易引擎。

### 核心能力

- **Black-Scholes 定价**：基于 Binance BTC 现货价格实时计算二元期权公允价值
- **双向 HFT**：买入低估的 YES token，卖出高估的持仓，自动循环套利
- **四态状态机**：`Empty → PendingBuy → Holding → PendingSell`，Pending 锁防止重复下单
- **单例架构**：全局唯一 PolyClient（鉴权 + 限速器）和 Binance WS，20+ 任务共享
- **资金安全**：Decimal 精度预算控制、迟滞熔断器（亏损 20% 冻结，恢复到 15% 解冻）、看门狗自动解锁
- **优雅停机**：SIGINT / SIGTERM → 全局取消令牌 → 逐 token 撤销挂单 → 最终 P&L 快照
- **限速保护**：Semaphore(8) + 429 指数退避，防止被交易所封禁
- **静默日志**：默认零噪音，只在 edge 接近开火线或真实下单时输出

### 本地运行

```bash
# 先确保 DRY_RUN=true（默认值）
cargo test && cargo run

# 启动事件监控（另一个终端）
curl -X POST http://localhost:3333/event/start \
  -H 'Content-Type: application/json' \
  -d '{ "event_slug": "你的事件slug", "volatility": 0.5, "snipe_size": 10.0 }'
```

当你对风控和账本逻辑完全有信心后，再将 `DRY_RUN` 改为 `false`，从小额资金开始实盘。

---

## License

MIT
