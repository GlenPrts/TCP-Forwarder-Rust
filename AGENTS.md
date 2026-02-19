# TCP Forwarder Rust - 智能体指南 (Agent Guide)

> **重要提示 / Important Instruction**:
> 本项目中，所有智能体（Agent）的**思考过程**（Thinking Process）和**最终输出**（Final Output）都必须使用**中文**。
> In this project, all Agent **thinking processes** and **final outputs** MUST be in **Chinese**.

此仓库包含一个使用 Rust 编写的高性能 TCP 转发器，专为 Cloudflare IP 优选设计。

## 项目概览 (Project Overview)

- **语言**: Rust (Edition 2021)
- **运行时**: Tokio (Async)
- **关键依赖**: `tokio`, `axum`, `reqwest`, `serde`, `tracing`, `anyhow`, `thiserror`, `json_comments`
- **架构**:
  - `src/main.rs`: 入口点，参数解析，优雅停机，后台扫描启动。
  - `src/server.rs`: TCP 转发逻辑，包含并发连接竞速机制和 TCP Keepalive。
  - `src/pool.rs`: 预连接池，后台预建立 TCP 连接并维持水位，消除握手延迟。
  - `src/scanner.rs`: IP 延迟扫描与评分，支持自适应三阶段扫描及后台定时扫描。
  - `src/state.rs`: 共享状态管理 (`IpManager`)。
  - `src/config.rs`: 配置加载与验证（使用 `ipnetwork` 解析 CIDR，支持 JSONC 格式）。
  - `src/web.rs`: Web 监控接口。
  - `src/utils.rs`: 通用工具函数（IP 生成等）。
  - `src/analytics.rs`: 数据中心统计分析。
  - `src/model.rs`: 核心数据结构定义。

## 构建与测试命令 (Build & Test Commands)

### 构建 (Build)
```bash
# 开发构建
cargo build

# 发布构建 (推荐用于生产环境)
cargo build --release
```

### 运行 (Run)
```bash
# 运行转发服务 (默认)
cargo run

# 带参数运行
cargo run -- --scan            # 运行 IP 扫描器
cargo run -- --scan-on-start   # 转发模式并立即扫描
cargo run -- --rank-colos      # 显示数据中心排名
```

### 测试 (Test)
```bash
# 运行所有测试
cargo test

# 运行特定测试
cargo test test_name_pattern

# 运行测试并显示输出
cargo test -- --nocapture
```

### 代码检查与格式化 (Lint & Format)
```bash
# 检查代码风格
cargo fmt --check

# 运行 Linter
cargo clippy -- -D warnings
```

## 代码风格与规范 (Code Style & Conventions)

### 通用 (General)
- **格式化**: 遵循标准 Rust 格式化 (`rustfmt`)。
- **命名**:
  - `PascalCase` 用于结构体、枚举、Trait。
  - `snake_case` 用于函数、变量、模块。
  - `SCREAMING_SNAKE_CASE` 用于常量。
- **注释**: 现有的文档注释 (`///`) 为**中文**。公共接口必须保持使用中文注释。内部逻辑注释可以使用英文或中文。

### 错误处理 (Error Handling)
- **应用逻辑**: 使用 `anyhow::Result` 和 `anyhow::Context` 便于错误传播。
- **库/核心逻辑**: 使用 `thiserror` 定义强类型错误枚举 (例如 `src/config.rs` 中的 `ConfigError`)。
- **禁止解包**: 避免使用 `.unwrap()` 或 `.expect()`，除非在测试中，或初始化阶段遇到不可恢复的致命错误时。

### 异步与并发 (Async & Concurrency)
- 所有异步操作使用 `tokio`。
- 处理多个异步源（取消、IO）时使用 `tokio::select!`。
- 跨任务共享状态使用 `Arc<T>`。
- 限制并发（如扫描器中）使用 `tokio::sync::Semaphore`。
- 优雅停机使用 `tokio_util::sync::CancellationToken`。
- 对于大规模并发任务（如扫描、连接池补充），优先使用 `futures::stream::FuturesUnordered` 结合 `StreamExt` 进行并行处理，而非手动 `tokio::spawn`。
- 计算密集型任务（如统计分析、排序）使用 `Rayon` 进行并行化处理，以充分利用多核性能。

### 日志 (Logging)
- 使用 `tracing` 库的宏: `error!`, `warn!`, `info!`, `debug!`, `trace!`。
- **不要**在应用日志中使用 `println!`；仅用于 CLI 输出（如 `--help`）。

### 配置 (Configuration)
- 配置从 `config.jsonc` 加载（支持 JSONC 格式，允许注释）。
- 使用 `serde` 进行序列化。
- 所有配置字段必须在 `AppConfig::validate` 中进行验证。

## 实现模式 (Implementation Patterns)

### 连接竞速 (Connection Racing)
服务器使用 "Happy Eyeballs" 风格的竞速机制 (`server.rs` 中的 `race_connections`) 来建立连接。它通过 `staggered_delay_ms` 实现阶梯式启动，并行尝试多个连接并使用第一个成功的连接，同时立即取消其他尝试。

### 状态管理 (State Management)
`IpManager` 是中心状态，线程安全（内部使用锁/映射），并通过 `Arc` 共享。它管理 IP 池、评分和持久化。

### 优雅停机 (Graceful Shutdown)
应用程序监听 `Ctrl+C`。收到信号时：
1. `CancellationToken` 被取消。
2. Server 和 Web 任务停止接受新连接。
3. 应用程序等待任务完成（带超时）。

### 网络健壮性 (Network Robustness)
- 确保为长连接（包括客户端和远程端）启用了 TCP Keepalive，以便及时检测死链。

## 代码规范补充 (Additional Code Conventions)

### 行宽与函数规模
- **行宽限制**: 单行代码长度不得超过 80 个字符。
- **函数规模**: 单个函数长度不得超过 60 行。
- **嵌套层级**: 代码逻辑嵌套深度不得超过 3 层。
- **文件规模**: 单个文件长度不得超过 1000 行。
- **逻辑拆分**: 一旦超出上述限制，必须进行逻辑拆分。

### 数值安全
- 除法运算使用 `checked_div` 防止除零错误。
- 浮点数排序使用 `total_cmp` 而非 `partial_cmp`，正确处理 NaN。

### 禁止 else
- 代码中严禁出现 `else` 或 `else if`。
- 应通过卫语句 (Guard Clauses)、提前返回 (Early Return) 重构逻辑。
