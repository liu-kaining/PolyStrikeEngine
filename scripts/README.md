# Scripts（请用虚拟环境运行）

**请务必在虚拟环境中运行 Python 脚本**，避免依赖污染系统环境。

## 1. 创建虚拟环境（首次）

```bash
cd scripts
python3 -m venv .venv
source .venv/bin/activate   # Windows: .venv\Scripts\activate
pip install -r requirements.txt
```

## 2. 之后每次运行前：激活虚拟环境

```bash
cd scripts
source .venv/bin/activate   # Windows: .venv\Scripts\activate
python late_to_tomorrow_harvester.py
```

输出 CSV：`scripts/late_to_tomorrow_full_trades.csv`。脚本会分页抓取、实时追加写入，结束后输出总交易额、持仓时长分布、最频繁市场排名。

若 Gamma API 的 `/profiles` 返回 401，可手动指定该用户的 proxy 钱包地址再跑：

```bash
export PROXY_WALLET=0x你的或目标用户的钱包地址
python late_to_tomorrow_harvester.py
# 或一行传参：
python late_to_tomorrow_harvester.py 0x...
```
