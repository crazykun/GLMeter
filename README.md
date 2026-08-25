# GLMeter

跨平台 GLM Coding Plan 配额托盘监控工具（Windows / macOS / Linux），用 Rust 编写。

在系统托盘实时查看：

- **5 小时额度**：已用 / 剩余百分比、下次重置时间及倒计时
- **每周额度**（Pro/Max 套餐）
- **MCP 月度调用额度**：用量明细（zread / search-prime / web-reader …）
- 套餐等级（Lite / Pro / Max）

## 特性

- ⚡ **激活额度**：新窗口未被使用时 API 不计算 `nextResetTime`，点击菜单里的「激活额度」会发送一条最小请求（`"1"`，约 20 tokens），立即激活 5 小时窗口并显示重置时间
- ↻ 定时自动刷新（默认 5 分钟，可配置），左键点击托盘立即刷新
- 🔧 配置热加载：修改配置文件后无需重启
- 🖥 `--check` 无界面模式，便于脚本化与调试

## 截图（菜单示意）

```
GLM Coding Plan · Lite
────────────────────────
5小时额度: 已用 6%（剩余 94%）
  ↻ 重置 今天 19:20（59分后）
────────────────────────
MCP 月额度: 11/100 次（已用 11%）
  · search-prime: 4 · zread: 7
  ↻ 重置 09-16 09:11
────────────────────────
⚡ 激活额度（发送 "1"）
↻ 立即刷新
⚙ 打开配置文件
✕ 退出
```

## 安装

从 [Releases](https://github.com/crazykun/GLMeter/releases) 下载对应平台的二进制；或自行编译：

```bash
cargo build --release
```

### Linux 依赖

需要 GTK3 与 libappindicator（多数桌面发行版已内置）：

```bash
# Debian / Ubuntu
sudo apt install libgtk-3-dev libayatana-appindicator3-dev
```

## 配置

首次运行会生成配置模板，路径：

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/glmeter/config.toml` |
| macOS | `~/Library/Application Support/glmeter/config.toml` |
| Windows | `%APPDATA%\glmeter\config.toml` |

```toml
# 智谱开放平台 API Key（id.secret 格式），必填
api_key = "xxxxxxxx.yyyyyyyy"

# 国内: https://open.bigmodel.cn  国际: https://api.z.ai
base_url = "https://open.bigmodel.cn"

# 激活额度时使用的模型
model = "glm-5.2"

# 激活请求的 max_tokens
max_tokens = 8

# 自动刷新间隔（秒），最小 60
interval_secs = 300
```

也可用环境变量 `GLM_API_KEY` / `GLM_BASE_URL` 覆盖（适合 CI / 临时使用）。

## 无界面模式

```bash
./glmeter --check               # 查询并打印当前配额
./glmeter --check --activate    # 先激活 5 小时窗口再查询
```

输出示例：

```
配置文件 : /home/jii/.config/glmeter/config.toml
端点     : https://open.bigmodel.cn
套餐等级 : lite
5小时额度: [█░░░░░░░░░░░░░░░░░░░░░░░] 已用 6%（剩余 94%）
  重置时间: 2026-08-25 19:20（59分后）
MCP 月额度: 11/100 次（已用 11%）
  · search-prime: 4
  · zread: 7
```

## 工作原理

| 用途 | 接口 |
|---|---|
| 配额查询 | `GET {base_url}/api/monitor/usage/quota/limit`（`Authorization: <api_key>`） |
| 激活额度 | `POST {base_url}/api/coding/paas/v4/chat/completions`（Bearer，发送 `"1"`） |

- `TOKENS_LIMIT` → 5 小时窗口（Pro/Max 另有每周窗口），含 `percentage` 与 `nextResetTime`
- `TIME_LIMIT` → MCP 月度额度

API Key 仅保存在本地配置文件中，本仓库不收集任何数据。

## 开发

```bash
cargo run            # 托盘模式
cargo run -- --check # 调试模式
cargo clippy && cargo fmt --check
```

## License

MIT
