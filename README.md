# Herdr Coordinator（塔台）

Herdr Coordinator 是一个面向多项目 Agent 协作的轻量塔台。它不接管项目内部的任务拆分和代码实现，只维护三个事实：有哪些项目、每个项目的机长是谁、项目当前处于什么状态。

项目通过 `herdr` CLI 连接当前 Herdr 会话，读取 Agent 实时状态并向机长转发指令；注册表和汇报收件箱持久化在本地文件中，因此塔台重启或上下文压缩后仍能恢复现场。

> 当前版本面向 macOS 和本地单用户 Herdr 会话。注册表是本地状态，不是 GitHub、数据库或多机同步服务。

## 核心模型

每个项目对应一个独立 fleet：

- **机长（commander）**：项目唯一决策者，负责拆任务、分派、验收和汇报。
- **副机长（reviewer）**：可选的建议性审查者。
- **乘务（crew）**：执行具体任务的 Agent。
- **塔台（coordinator）**：只登记项目、转发指令和汇总状态，不保存项目实现细节。

状态以 `~/.herdr-coordinator/` 下的注册表为准，并通过 `herdr agent list` 与实时会话对账。机长在完成、卡住或需要用户决策时主动写入收件箱，塔台无需持续轮询每个 Agent。

## 组成

| 路径 | 作用 |
| --- | --- |
| `AGENTS.md` | 塔台的操作约定、fleet 编制和故障处置规则 |
| `fleet` | 注册表与收件箱命令行工具 |
| `dashboard.sh` | 只读刷新式终端看板 |
| `tower/` | 基于 Ratatui 的交互式终端看板 |

## 环境要求

- 已安装并启动 Herdr，`herdr` 命令位于 `PATH`
- 命令在 Herdr 管理的 pane 内运行，环境变量 `HERDR_ENV=1`
- Python 3.9 或更高版本，用于 `fleet`
- Bash，用于 `dashboard.sh`
- Rust stable 和 Cargo，仅在构建 `tower` 时需要
- `curl`，仅在启用可选的 Agent 进展摘要时需要

可先检查运行环境：

```bash
test "${HERDR_ENV:-}" = 1
herdr agent list
python3 --version
```

## 快速开始

```bash
git clone https://github.com/sm-yjr/herdr-coordinator.git
cd herdr-coordinator

# 初始化本地注册表和收件箱
./fleet init

# 查看登记状态并与 Herdr 实时状态对账
./fleet list

# 读取尚未处理的机长汇报
./fleet inbox
```

登记一个已经在 Herdr 中启动的项目 fleet：

```bash
./fleet register demo \
  --tab w1:t2 \
  --commander demo-cmd \
  --cwd /path/to/demo

./fleet set-status demo working "正在实现首个版本"
./fleet list
```

机长在关键节点写入汇报：

```bash
./fleet report demo need_decision "需要确认是否保留旧配置格式"
./fleet report demo done "实现和测试均已完成"
```

项目结束并关闭对应 Herdr tab 后，从注册表移除：

```bash
./fleet unregister demo
```

## 状态与收件箱

注册表支持以下状态：

| 状态 | 含义 |
| --- | --- |
| `working` | 项目正在执行任务 |
| `idle` | 机长空闲，可以接收指令 |
| `done` | 当前目标已经完成 |
| `blocked` | 项目无法继续，需要排查阻塞原因 |
| `need_decision` | 需要用户提供决策或确认 |

`./fleet sync` 会把 Herdr 的实时状态同步到注册表，但不会自动覆盖 `need_decision`。这是有意的：需要拍板的问题只能由明确的机长汇报或人工处理来关闭。

`./fleet inbox` 默认只显示未读消息，并在读取后推进游标。使用 `./fleet inbox --all` 可以查看完整历史。

## 终端看板

不构建 Rust 程序时，可使用刷新式看板：

```bash
./dashboard.sh       # 默认每 3 秒刷新
./dashboard.sh 5     # 每 5 秒刷新
```

交互式 Ratatui 看板的运行方式：

```bash
cargo run --release --manifest-path tower/Cargo.toml
```

常用操作：

- `j` / `k` 或方向键：切换项目
- 鼠标点击：展开或折叠项目
- `Enter`：全部展开或全部折叠
- `r` 或点击收件箱：标记汇报为已读
- `d`：切换收件箱自动派单
- `q` 或 `Esc`：退出

看板每 5 秒刷新一次 Herdr 和注册表状态。自动派单只会把新汇报发给空闲的塔台管制员；没有可用管制员时，消息保留在队列中等待下一轮。

## 可选的进展摘要

交互式看板可以读取 Agent 终端尾部内容，并通过 DashScope 兼容接口生成一句中文进展摘要。此功能未配置密钥时自动跳过，不影响其他功能。

```bash
export DASHSCOPE_API_KEY="your-api-key"
export TOWER_SUMMARY_MODEL="qwen3.7-flash"  # 可选
cargo run --release --manifest-path tower/Cargo.toml
```

API Key 只从环境变量读取，不会写入项目文件。启用后，Agent 终端的部分文本会发送给所配置的模型服务；涉及敏感代码或日志时应关闭此功能。

## 本地数据

运行时数据写入 `~/.herdr-coordinator/`：

```text
fleets.json       项目注册表
inbox.jsonl       机长汇报历史
inbox.cursor      收件箱已读位置
dispatch.json     自动派单位置与最近目标
summaries.json    进展摘要缓存
```

这些文件不属于 Git 仓库。备份或迁移时，应把整个目录视为一个一致的数据集。手工编辑前先退出看板，避免与运行中的写入发生冲突。

## 开发与验证

```bash
python3 -m py_compile fleet
bash -n dashboard.sh
cargo check --manifest-path tower/Cargo.toml
```

项目目前没有后台守护进程、远程认证或多机并发写入保护。它适合由一个操作者管理一个本地 Herdr 会话；不适合作为共享控制面直接暴露到网络。

## License

本项目采用 [MIT License](LICENSE)。
