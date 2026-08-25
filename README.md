# Herdr Coordinator（塔台）

Herdr Coordinator 是一个面向长时间运行、跨项目 coding-agent fleet 的人机协作控制面。它不替代 Claude Code、Codex、Pi 等执行 Agent，也不接管项目内部的任务拆分；它只维护项目、机长、可信状态、用户决策和需要人类介入的事项。

核心目标不是“多开几个 Agent”，而是降低一个操作者监督多个项目时的注意力成本：哪一个项目最需要介入、为什么、应该由谁采取什么动作。

## 核心模型

每个项目对应一个 fleet：

- **机长（commander）**：项目唯一负责人，持有项目上下文、拆任务、派乘务、验收并向塔台汇报。
- **副机长（reviewer）**：可选的建议性审查者。
- **乘务（crew）**：执行具体任务。
- **塔台（coordinator）**：登记项目、观察运行时、记录声明与证据、路由指令、汇总需要人类介入的事项。

塔台将四类事实分开保存：

```text
Herdr snapshot / event ──> runtime.json   observed：Agent 在线、working/blocked/idle
commander fleet report ──> claims.json    claimed：机长声称项目处于什么状态
fleet verify command   ──> claims.json    verified：塔台进程实际执行命令得到的证据
fleet ask / accept     ──> decisions.json governed：用户决策与最终验收
```

实时 `blocked` 不等于正式 `need_decision`；机长声称 `done` 也不等于已经通过验证或用户验收。

## 作为 Herdr plugin 安装

要求：Herdr 0.8.0 或更高版本，主机为 macOS 或 Linux。正式安装优先使用 GitHub Release 中经过校验的预编译 Rust 二进制；没有匹配产物时才需要本机 Rust toolchain。

```bash
herdr plugin install sm-yjr/herdr-coordinator
herdr plugin action invoke open --plugin sm-yjr.herdr-coordinator
```

本地开发时：

```bash
git clone https://github.com/sm-yjr/herdr-coordinator.git
cd herdr-coordinator
cargo build --release --locked --manifest-path tower/Cargo.toml
mkdir -p bin && cp tower/target/release/herdr-coordinator bin/
herdr plugin link "$(pwd)"
herdr plugin action invoke open --plugin sm-yjr.herdr-coordinator
```

Plugin 提供：

- Herdr 启动后的 snapshot 对账；
- pane / tab / workspace / Agent 状态事件触发的增量重对账；
- `Fleet Control Tower` overlay；
- `open` 与 `reconcile` 两个 action；
- Herdr 管理的独立 config/state 目录；
- 首次启动时从独立模式的 `~/.herdr-coordinator/` 一次性导入耐久状态。

导入只复制注册表、claims、decisions、inbox 等耐久数据，不复制旧的 `runtime.json`、watcher PID 或日志。`legacy-import.json` 记录来源和导入清单。

Plugin startup 是一次性恢复，不运行常驻 daemon。事件 hook 可能并发执行，因此每个 hook 都在文件锁内重新读取一次权威 snapshot，而不是直接相信可能乱序到达的事件 payload。snapshot 调用默认 15 秒超时，可用 `HERDR_SNAPSHOT_TIMEOUT` 调整。

仓库根目录的 `./fleet` 是统一入口：plugin 进程直接使用 Herdr 注入的 state 目录；普通 commander/crew pane 没有 plugin 环境变量时，入口会自动发现已安装 plugin 的 state 目录。因此机长汇报、Ratatui overlay、event hook 和命令行查看共享同一个状态源，不会形成两份状态。生产逻辑全部位于 `tower/src/`，由同一个 `herdr-coordinator` Rust 二进制提供。

Ratatui 浮层以“等你拍板 → 需要介入 → 项目 → 最近事件”为信息层级，支持方向键或 `j/k` 选择、`Enter` 展开项目、`r` 重新对账、`a` 重建注意力、`m` 标记已读、`q` 关闭。

## 快速开始

```bash
./fleet init

./fleet register demo \
  --tab w1:t2 \
  --commander demo-cmd \
  --cwd /path/to/demo

./fleet list
./fleet attention
```

在未安装为 plugin 的兼容模式中，可以运行长连接 watcher：

```bash
./fleet watch-start
./fleet watch-status
```

Plugin 模式不需要 `watch-start`。

## 项目汇报与可信完成

机长汇报业务状态：

```bash
./fleet report demo working "正在实现配置迁移" --confidence 0.8

./fleet report demo done "迁移逻辑和单元测试已完成" \
  --confidence 0.92 \
  --evidence "commit:7596450" \
  --evidence "pr:42"
```

`--evidence` 是机长提供的线索，默认只是未验证声明。机器验证必须由塔台显式执行：

```bash
./fleet verify demo \
  --label unit-tests \
  -- cargo test --manifest-path tower/Cargo.toml
```

验证命令的退出码、耗时以及 stdout/stderr 尾部会写入本地 `claims.json`。不要让验证命令把密钥或敏感日志输出到终端。

也可以指定某个 claim：

```bash
./fleet claims demo --all
./fleet verify demo --claim <claim-id> --label cargo-check -- \
  cargo check --manifest-path tower/Cargo.toml
```

验证通过后，claim 从 `reported` 变为 `verified`。用户验收：

```bash
./fleet accept demo --claim <claim-id> --note "验收通过"
```

默认只有 `done + verified` 的 claim 可以验收。确需人工覆盖时使用 `--force`，并用 `--note` 明确记录接受风险的原因：

```bash
./fleet accept demo --claim <claim-id> --force --note "接受已知风险"
```

## 用户决策

机长遇到真正需要用户拍板的问题时：

```bash
./fleet ask demo "是否保留旧配置格式？" \
  --option "保留，兼容旧版本" \
  --option "升级，只支持新格式"
```

塔台查看和解决：

```bash
./fleet decisions
./fleet resolve <decision-id> "保留旧格式"
```

兼容旧写法：

```bash
./fleet report demo need_decision "是否保留旧配置格式？"
```

## 注意力队列

`fleet attention` 不展示所有变化，而是按“此刻人类介入能释放多少后续工作”排序：

```bash
./fleet attention
./fleet attention --json
```

当前识别：

| 类型 | 含义 | 默认负责人 |
| --- | --- | --- |
| `decision_required` | 存在未解决的正式决策 | user |
| `runtime_disconnected` | 无法获得 Herdr 权威运行时状态 | coordinator |
| `stale_blocked` | 机长阻塞超过 10 分钟且未创建决策 | coordinator |
| `observed_blocked` | 机长实时 blocked，但尚无正式决策 | coordinator |
| `commander_offline` | 项目仍 working/blocked，但机长离线 | coordinator |
| `reported_done_unverified` | 机长声称完成，但没有通过的机器证据 | coordinator |
| `ready_for_acceptance` | 已验证，等待用户最终验收 | user |
| `project_idle_mismatch` | 项目仍 working，但机长长期 idle | coordinator |

每条 intervention 都包含确定性的类型、优先级、原因、负责人和建议命令。投影同时写入 `attention.json`，供 overlay 和其他工具读取。

## 指令路由

塔台只把完整目标发给注册表中的唯一机长：

```bash
./fleet route demo "检查失败的 CI，修复后给出验证证据"
```

调用方不能指定副机长或乘务。任务拆分、worktree、测试策略、合并和发布仍由机长负责。

## 常用命令

```text
fleet init
fleet register / unregister
fleet list
fleet route
fleet report / claims / verify / accept
fleet ask / decisions / resolve
fleet attention
fleet inbox
fleet sync
fleet plugin-reconcile / plugin-event
fleet watch-start / watch-status / watch-stop
```

## 本地数据

状态目录按以下优先级选择：Herdr 注入的 `HERDR_PLUGIN_STATE_DIR`、显式 `HERDR_COORDINATOR_HOME`、已安装 plugin 的 Herdr state 目录，最后才回退到 `~/.herdr-coordinator/`。所有角色都应调用根目录 `./fleet`，以确保共享同一个状态源。

```text
fleets.json       项目注册表与当前 claim 指针
claims.json       状态声明、机器证据、验收记录
decisions.json    待拍板事项与解决记录
runtime.json      Herdr snapshot/event 的实时投影
attention.json    排序后的人工介入队列
legacy-import.json 独立模式状态的一次性导入记录
inbox.jsonl       状态、验证、决策和验收事件历史
inbox.cursor      收件箱已读位置
watch.pid/log     独立 watcher 的兼容状态
```

写入使用进程锁、临时文件、`fsync` 和原子替换。当前实现适合一个本地操作者管理一个 Herdr 会话，不提供多用户认证或网络服务。

## 开发与验证

```bash
cargo fmt --manifest-path tower/Cargo.toml --all --check
cargo clippy --manifest-path tower/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path tower/Cargo.toml --all-targets
cargo build --manifest-path tower/Cargo.toml --release --locked
bash -n fleet dashboard.sh scripts/fetch-or-build.sh
```

CI 在 Linux 和 macOS 上执行格式、Clippy、测试和 release build。

## 版本与发布

`herdr-plugin.toml` 与 `tower/Cargo.toml` 使用同一个 SemVer。发布时只推送匹配版本的 tag：

```bash
git tag v0.3.0
git push origin v0.3.0
```

GitHub Actions 会拒绝 tag、plugin 版本和 crate 版本不一致的发布。验证通过后，它会构建 macOS/Linux 的 Intel 与 ARM 四个平台，生成 `checksums.txt` 并创建 GitHub Release。`herdr plugin install` 的 build hook 会下载并校验对应二进制；Release 缺失或平台不匹配时才回退到本地 `cargo build --locked --release`。

完整发布检查和失败处理见 [`docs/RELEASING.md`](docs/RELEASING.md)。

## 设计边界

Herdr Coordinator 是监督控制面，不是任务 orchestrator。以下信息不进入塔台注册表：

- 乘务级 task DAG；
- worktree 和分支分配；
- 测试与构建流水线定义；
- merge queue 和发布步骤；
- 项目实现细节。

这些属于机长的执行层。塔台只保存跨上下文恢复所需的最小控制面事实。

## License

MIT
