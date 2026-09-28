# GLMeter

跨平台 GLM Coding Plan 配额托盘监控工具（Windows / macOS / Linux），用 Rust 编写。

![GLMeter](assets/glmeter.png)

在系统托盘实时查看：

- **5 小时额度**：已用 / 剩余百分比、下次重置时间及倒计时
- **每周额度**（Pro/Max 套餐）
- **MCP 月度调用额度**：用量明细（zread / search-prime / web-reader …）
- 套餐等级（Lite / Pro / Max）

## 特性

- ⚡ **激活额度**：新窗口未被使用时 API 不计算 `nextResetTime`，点击菜单里的「激活额度」会发送一条最小请求（`"1"`，约 20 tokens），立即激活 5 小时窗口并显示重置时间；**未生效自动重试**（1 分钟后重试，最多 3 次）
- ⏰ **定时激活**，两种模式可叠加：
  - `auto_activate`（默认开）：窗口未激活或重置时间一到就自动激活，倒计时永不断档
  - `activate_at` 定点激活：每天在配置的时刻（如 9:00、14:00）自动激活一次
- 📊 **进度条**：菜单与悬停提示中用块字符直观展示剩余额度
- ↻ **自动刷新**：默认每 5 分钟拉取一次配额，间隔可调（最小 1 分钟），可对齐整分网格；左键点击托盘立即刷新
- ⏱ **倒计时实时**：菜单每 30 秒用当前数据重绘一次（不发请求），「xx分后」不会停在两次刷新之间
- 🎁 **重置卡**：菜单直接显示可用张数，点击弹窗确认后使用最早过期的一张（5h / 周额度），免开官网
- 🔔 **重置卡提醒**：到期提醒（剩余 24h / 6h / 1h 各提醒一次，阈值可配）+ 变动提醒（新增卡到账；减少时能区分「已使用」与「过期作废」——依据 `expireTime` 是否已到和上次用卡时间是否变化，GLMeter 自己用卡不提醒），支持系统桌面通知与群机器人 Webhook（`[notify]` 配置段，单个 `hook_url` 按域名自动识别企微 / 飞书 / 钉钉）；提醒状态本地持久化，重启不重复提醒
- 🆕 **更新检查**：启动后自动检查一次 GitHub 最新 Release，此后每 24 小时一次；也可随时点菜单「检查更新」。发现新版本时桌面通知/群机器人提醒，菜单出现「🆕 新版本 vX 可用 · 查看更新」一键跳转对应 Release 页。仅请求 GitHub 公开 API，不发送任何本机数据
- ℹ **关于窗口**：菜单底部「关于 GLMeter」弹原生窗口——GLMeter logo + 版本号 + 简介，按钮：GitHub 地址 / GLM 官网（用量统计）/ GLM 注册 / 确定，点入口按钮在浏览器打开对应页面
- 🔧 配置热加载：修改配置文件后无需重启
- 🖥 `--check` 无界面模式，便于脚本化与调试（Windows 下 GUI 程序在终端运行时自动挂回控制台输出）

## 截图（菜单示意）

```
GLM Coding Plan · Lite 套餐
────────────────────────
5小时额度 ████████░░░░ 剩余 70%
  ↻ 重置 今天 19:20（17分后）
────────────────────────
MCP 月额度 ██████████░░ 13/100 次
  · search-prime: 5 · zread: 7
  ↻ 重置 09-16 09:11
────────────────────────
↩ 重置卡恢复5小时额度（×6 · 最早 10-01 23:59 过期）
────────────────────────
⚡ 激活额度（发送 "1"）
↻ 立即刷新（更新于 19:03:12）
⚙ 打开配置文件
────────────────────────
🆕 检查更新（✓ 已是最新 v0.2.8）
ℹ 关于 GLMeter v0.2.8
✕ 退出
```

## 安装

**Homebrew（macOS / Linux）：**

```bash
brew install crazykun/ailater/glmeter
```

**安装包**（从 [Releases](https://github.com/crazykun/GLMeter/releases) 下载）：

| 平台 | 安装包 | 说明 |
|---|---|---|
| Linux | `glmeter_<版本>_amd64.deb` | `sudo dpkg -i` 安装；二进制为 musl 静态链接，任何 x86_64 发行版可用 |
| Linux | `glmeter-linux-x86_64.tar.gz` | 免安装，解压即用 |
| macOS | `glmeter-macos-<arch>.dmg` | .app bundle，拖入 Applications；启动后常驻菜单栏 |
| macOS | `glmeter-macos-<arch>.tar.gz` | 免安装（Homebrew 使用此格式） |
| Windows | `glmeter-<版本>-setup.exe` | Inno Setup 安装器，可选开机自启 |
| Windows | `glmeter-windows-x86_64.zip` | 免安装，解压即用 |

或自行编译：

```bash
cargo build --release
```

### Linux 依赖

**无**。Linux 后端使用 [ksni](https://crates.io/crates/ksni)（StatusNotifierItem 协议纯 Rust 实现），不依赖 GTK/libappindicator，glibc 即可运行。

### Linux 已知说明

- 托盘基于 StatusNotifierItem(DBus) 协议（ksni 纯 Rust 实现）：悬停显示详情提示，左键点击立即刷新，托盘文字由 `tray_title` 模板自定义
- Deepin（dde-tray-loader / dde-dock）的悬停提示只读 SNI ToolTip 的 title 单字段，GLMeter 已把全部明细合并进该字段以兼容；KDE Plasma 双字段都渲染，因此 description 留空避免重复
- Deepin 的 `xdg-open` 经 `dde-open` 打开 URL 时会丢失链接参数（只激活浏览器不跳转）；GLMeter 在 Linux 上优先用 `gio open` 打开链接规避，无 gio 时退回 xdg-open
- dock/panel 重启导致 watcher 离线时自动重连，无需重启 GLMeter
- GLMeter 使用单实例锁（`instance.lock`），重复启动会直接退出
- 若托盘长时间不显示，可尝试重启 dock/panel（如 Deepin 的 dde-dock）后重新运行

## 配置

首次运行会生成配置模板，路径：

| 平台 | 路径 |
|---|---|
| Linux | `~/.config/glmeter/config.toml` |
| macOS | `~/Library/Application Support/glmeter/config.toml` |
| Windows | `%APPDATA%\glmeter\config.toml` |

```toml
# ── 基础 ──────────────────────────────────────────────

# 智谱开放平台 API Key（id.secret 格式），必填
api_key = "xxxxxxxx.yyyyyyyy"

# 国内: https://open.bigmodel.cn  国际: https://api.z.ai
base_url = "https://open.bigmodel.cn"

# 激活额度时使用的模型 / 请求的 max_tokens
model = "glm-5.2"
max_tokens = 8

# ── 自动刷新（拉取配额显示）──────────────────────────
# 刷新间隔（秒），最小 60（= 每 1 分钟）
interval_secs = 300

# 间隔对齐起点（"HH:MM"，留空则从启动时刻滚动计时）。
# 例如 interval_secs = 300 + "00:00" → 每天 00:00/00:05/00:10… 整点网格刷新
refresh_align = ""

# ── 定时激活（发送最小请求激活 5h 窗口）───────────────
# 模式一：auto，窗口未激活 / 重置时间一到就自动激活（默认开启）
auto_activate = true

# 模式二：每天定点激活，可配多个时刻（本地时区）：
#   activate_at = ["09:00"]            → 每天早上 9 点激活
#   activate_at = ["09:00", "14:00"]   → 每天 9 点和 14 点各激活一次
# 激活未生效时 1 分钟后自动重试，最多 3 次
activate_at = []

# ── 托盘 ──────────────────────────────────────────────

# 托盘显示文字模板（Linux 悬停 Title / macOS 菜单栏文字），支持变量：
#   {level} {5h_used} {5h_left} {5h_reset} {5h_countdown}
#   {weekly_used} {weekly_left} {mcp_used} {mcp_total} {mcp_left}
tray_title = "GLM {5h_left}%"

# ── 重置卡提醒 ────────────────────────────────────────
[notify]

# 系统桌面通知（Windows toast / macOS 通知中心 / Linux notify-send），默认开
desktop = true

# 到期提醒阈值（小时）：重置卡剩余有效期 ≤ 该值时提醒一次，
# 每张卡每档只提醒一次（防重复），空列表 [] 关闭到期提醒
expire_hours = [24, 6, 1]

# 群机器人 Webhook（可选，留空 = 不发送），按域名自动识别机器人类型：
#   qyapi.weixin.qq.com   → 企业微信群机器人
#   open.feishu.cn        → 飞书自定义机器人（国际版 open.larksuite.com 同样支持）
#   oapi.dingtalk.com     → 钉钉自定义机器人
# 消息以「GLMeter」开头，钉钉自定义关键词安全设置可直接填 GLMeter；
# 钉钉选「加签」安全设置时另填 dingtalk_secret（其余平台忽略该项）
hook_url = ""          # 如 https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx
dingtalk_secret = ""
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
| 重置卡余额 / 使用 | `GET/POST {base_url}/api/biz/customer-package-reset`（Bearer） |
| 更新检查 | `GET https://api.github.com/repos/crazykun/GLMeter/releases/latest`（GitHub 公开 API，每 24h 一次） |

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
