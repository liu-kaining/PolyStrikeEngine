## PolyStrike Engine

High-frequency sniper engine for Polymarket. It combines:

- **Binance** as a price oracle (BTCUSDT `@bookTicker`)
- **Polymarket CLOB** as the execution venue
- **Axum HTTP API** as the SaaS control plane (start/stop per-market strategies)
- **Tokio + DashMap + Decimal** for low-latency, concurrency-safe risk and inventory management

The engine is designed to be **production-grade**:

- Global, shared **`RiskGuard`** with `Decimal`-precision budget control
- In-memory **`InventoryManager`** with exact P&L and exposure tracking
- **Dry-run mode** enabled by default (`DRY_RUN=true`) to prevent accidental live trading
- Per-market sniper tasks that can be started/stopped dynamically via HTTP without restarting the process

---

### Architecture Overview

- `main.rs`
  - Loads `.env` and `AppConfig`
  - Constructs global:
    - `Arc<InventoryManager>`
    - `Arc<RiskGuard>` (global budget + per-market exposure caps)
    - `Arc<StrategyRegistry>` (token_id → `CancellationToken`)
  - Spawns the Axum HTTP API on port **3333**
  - Waits for `Ctrl+C`, then prints a final exposure/P&L snapshot and exits gracefully

- `src/api/server.rs`
  - `GET /status` — health check
  - `GET /exposure` — returns all market exposures (`MarketExposure`) from `InventoryManager`
  - `POST /strategy/start`
    - Body: `StartStrategyReq { token_id, strike_price, snipe_size }`
    - Atomically registers `token_id` in `StrategyRegistry`
    - Spawns `engine::run_sniper_task(...)` for that market
  - `POST /strategy/stop`
    - Body: `StopStrategyReq { token_id }`
    - Cancels and removes the corresponding `CancellationToken`

- `src/engine.rs`
  - `run_sniper_task(token_id, strike_price, snipe_size, inventory, risk_guard, cancel_token, strategies)`
    - Subscribes to:
      - Binance `@bookTicker` for `BINANCE_SYMBOL`
      - Polymarket orderbook for the given `token_id`
    - Uses `SniperStrategy` to evaluate when to fire
    - Uses `CancellationToken` + RAII guard (`StrategyGuardDrop`) to:
      - Stop when cancelled
      - Remove `token_id` from `StrategyRegistry` on any exit path
    - Prints **heartbeat logs**:
      - `[Engine Heartbeat] Binance Mid: $... | Poly Ask: ... | Poly Bid: ...`
  - `try_fire(...)`
    - Reserves budget via `RiskGuard::reserve_budget`
    - If `DRY_RUN=true`:
      - Logs `[DRY RUN] Would have executed snipe order. Releasing budget...`
      - Exits, RAII guard releases budget
    - If live:
      - Executes `PolyClient::execute_snipe_order`
      - On success: records fill in `InventoryManager` and **commits** budget
      - On failure: automatically **releases** reserved budget

- `src/risk/guard.rs`
  - `RiskGuard` with:
    - `reserve_budget` / `release_budget` (using `Decimal`)
    - `check_market_exposure` (per-market YES/NO caps)
  - Unit tests:
    - Budget reserve/release
    - Budget exhaustion
    - Market exposure limits
    - Concurrent reservations never exceed `max_budget`

- `src/risk/inventory.rs`
  - `InventoryManager` with an internal `Decimal`-based ledger and `MarketExposure` API struct
  - Methods:
    - `apply_fill` / `add_fill`
    - `get_global_exposure` / `get_global_exposure_excluding`
    - `get_net_exposure`, `get_exposure`, `snapshot_all`
    - `get_last_local_fill_timestamp`
  - Unit tests:
    - P&L and size accumulation across multiple fills
    - Timestamp update correctness
    - Concurrent updates via `DashMap`

---

### Configuration (.env)

Environment variables (examples):

```env
BINANCE_SYMBOL=btcusdt
POLY_TOKEN_ID=64087619211543545431479218048939484178441767712621033463416084593776314629222
CHAIN_ID=137
PK=0xYOUR_PRIVATE_KEY
FUNDER_ADDRESS=0xYOUR_FUNDER_ADDRESS

MAX_BUDGET=500.0
MAX_EXPOSURE_PER_MARKET=0.0
RECONCILIATION_BUFFER_SECONDS=8.0

# Safety default: DRY_RUN=true (no real orders)
DRY_RUN=true
# Set to false or 0 to enable live trading (double-check before changing!)
# DRY_RUN=false
```

**Dry run semantics**

- If `DRY_RUN` is **unset**, or set to anything other than `"false"` / `"0"` → `dry_run = true`
- To enable **live trading**, you must explicitly set:
  - `DRY_RUN=false` or `DRY_RUN=0`

---

### Running the Engine

1. Ensure dependencies are installed (Rust stable, OpenSSL/clang, etc.).
2. Create your `.env` based on the example above.
3. Run tests:

```bash
cargo test
```

4. Start the engine:

```bash
cargo run
```

The process will:

- Start the HTTP API on `http://0.0.0.0:3333`
- Wait for strategy start/stop commands
- Exit gracefully on `Ctrl+C`, printing final exposure snapshot

---

### SaaS API Usage

**Start a sniper strategy for a Polymarket token**

```bash
curl -X POST http://localhost:3333/strategy/start \
  -H 'Content-Type: application/json' \
  -d '{
    "token_id": "64087619211543545431479218048939484178441767712621033463416084593776314629222",
    "strike_price": 1000.0,
    "snipe_size": 10.0
  }'
```

**Stop a sniper strategy**

```bash
curl -X POST http://localhost:3333/strategy/stop \
  -H 'Content-Type: application/json' \
  -d '{
    "token_id": "64087619211543545431479218048939484178441767712621033463416084593776314629222"
  }'
```

**Check engine status**

```bash
curl http://localhost:3333/status
```

**Inspect current exposures**

```bash
curl http://localhost:3333/exposure
```

---

## 中文说明 (Chinese Guide)

PolyStrike Engine 是一套面向 Polymarket 的高频“狙击引擎”，核心设计目标是：

- **资金安全优先**：全局唯一的 `RiskGuard`，用 `Decimal` 精度做预算与敞口控制，防止超卖与精度丢失。
- **内存优先账本**：`InventoryManager` 只在内存里维护仓位和 P&L，允许在撮合级别做实时风控。
- **SaaS 化控制台**：通过 HTTP API 动态启动/停止指定 token 的狙击任务，而无需重启进程。
- **默认模拟盘**：`DRY_RUN=true` 默认开启，避免误入实盘；只有显式改成 `false/0` 才会真实下单。

### 关键模块

- `main.rs`
  - 初始化全局 `InventoryManager` / `RiskGuard` / `StrategyRegistry`
  - 启动 Axum HTTP API（端口 3333）
  - 捕获 `Ctrl+C`，打印最终对账单（所有市场净敞口 + realized P&L）

- `src/api/server.rs`
  - `POST /strategy/start`：按 `token_id` 启动单独的狙击任务
  - `POST /strategy/stop`：按 `token_id` 停止对应任务
  - 所有任务都注册在 `StrategyRegistry` 中，通过 `CancellationToken` 做优雅退出

- `src/engine.rs`
  - 每个 `token_id` 启一个 `run_sniper_task`：
    - 监听 Binance BTCUSDT 和对应 Polymarket 订单簿
    - 按 `SniperStrategy` 的逻辑判断是否有 edge
    - 在真正下单前通过 `RiskGuard::reserve_budget` 预扣预算
    - 在模拟盘模式下仅打印 `[DRY RUN]` 日志并释放预算

- `src/risk/*`
  - `RiskGuard`：全局预算 + 单市场敞口控制；提供单元测试验证所有边界条件。
  - `InventoryManager`：维护每个市场的 yes/no 数量、pending notionals、realized P&L、最近成交时间戳，并提供并发安全的快照。

### 本地运行（推荐先模拟盘）

1. 根据 `.env` 示例填好环境变量，确保 **先保持 `DRY_RUN=true`**。
2. 跑一遍测试：

```bash
cargo test
```

3. 启动引擎：

```bash
cargo run
```

4. 通过 `curl` 或 Postman 调用 `/strategy/start` / `/strategy/stop` 来启停策略。

当你对所有风控与账本逻辑有足够信心后，再将 `DRY_RUN` 改为 `false`，并在小额资金下做真实回测。*** End Patch```}"/>