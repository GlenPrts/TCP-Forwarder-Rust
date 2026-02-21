# TCP Forwarder Rust

这是一个高性能的 TCP 转发器，旨在通过优选 IP（如 Cloudflare）转发流量。它采用扫描与转发分离的设计，结合基于 CIDR（IP 段）的智能评分与并发连接竞速机制，提供低延迟、高可靠的 TCP 转发服务。

## 核心特性

*   **基于 CIDR 的智能扫描**: 专门的扫描模式 (`--scan`)，自动将大网段拆分为子网（/24），并对每个子网进行采样评分，筛选优质 IP 段。
*   **自适应扫描策略**: 新增三阶段自适应扫描（广域稀疏扫描 -> 热点区域分析 -> 重点区域精细扫描），大幅提升扫描效率，快速定位优质网段。
*   **灵活的转发策略**: 支持基于评分的 Top K% 筛选，并在转发时采用随机选段 + 随机选 IP 的两级随机策略，有效均衡负载并避免拥塞。
*   **持久化存储**: 扫描结果以子网为单位保存到本地文件，转发服务启动时直接加载。
*   **并发连接竞速**: 转发流量时，同时向多个优选 IP 段内的随机 IP 发起连接，使用最快建立的连接，显著降低握手延迟。
*   **预连接池**: 后台预建立 TCP 连接并维持水位，客户端请求时直接复用，消除握手延迟。池空时自动降级为竞速模式。
*   **Web 状态监控**: 内置 Web 服务器，实时查看当前 IP 段池状态、质量评分和连接池统计。
*   **故障自动转移**: 智能处理连接失败，自动尝试备用 IP。
*   **后台定时扫描**: 支持在转发流量的同时，后台定时执行 IP 扫描任务，持续更新优选 IP 池。
*   **启动自检**: 可选 `--scan-on-start` 参数，确保服务启动时立即拥有最新的 IP 数据。
*   **JSONC 配置**: 配置文件支持注释（`config.jsonc`），方便文档化和维护。

## 快速开始

### 1. 环境准备

确保已安装 Rust 编程语言环境。如果没有，请访问 [rustup.rs](https://rustup.rs/) 安装。

### 2. 配置文件

在项目根目录下创建或修改 `config.jsonc` 文件（支持注释）：

```jsonc
{
  // 转发目标端口（Cloudflare HTTPS）
  "target_port": 443,
  // 限定数据中心代码，如 ["SJC", "LAX"]；空数组则不限制
  "target_colos": [],
  "cidr_list": [
    "104.16.0.0/12",
    "172.64.0.0/13"
  ],
  "bind_addr": "127.0.0.1:8080",
  "web_addr": "127.0.0.1:3000",
  "trace_url": "http://engage.cloudflareclient.com/cdn-cgi/trace",
  "asn_url": "https://asn.0x01111110.com/13335?4",
  "ip_store_file": "subnet_results.json",
  "selection_top_k_percent": 0.3,
  "selection_random_n_subnets": 3,
  "selection_random_m_ips": 2,
  // Happy Eyeballs 竞速连接的交错延迟（毫秒）
  "staggered_delay_ms": 200,
  "scan_strategy": {
    "type": "adaptive",
    "initial_scan_mask": 20,
    "initial_samples_per_subnet": 5,
    "promising_subnet_percent": 0.2,
    "focused_samples_per_subnet": 3
  },
  "background_scan": {
    "enabled": true,
    "interval_secs": 3600,
    "concurrency": 50
  },
  // 预连接池：后台预建立 TCP 连接，消除握手延迟
  "connection_pool": {
    "enabled": true,
    "pool_size": 16,
    "min_idle": 3,
    "max_idle_secs": 12,
    "refill_interval_ms": 4000
  },
  "tcp_keepalive": {
    "time_secs": 60,
    "interval_secs": 10
  },
  // 评分权重：调整延迟、抖动、丢包对评分的影响
  "scoring": {
    "base_score": 100.0,
    "latency_penalty_per_10ms": 1.0,
    "jitter_penalty_per_5ms": 1.0,
    "loss_penalty_per_percent": 50.0
  }
}
```

**配置项说明：**

*   `target_port`: 目标服务的端口（例如 Cloudflare 节点的 443 端口）。
*   `target_colos`: 期望的数据中心代码列表（如 "SJC", "LAX"）。如果留空 `[]`，则不限制数据中心。
*   `cidr_list`: 初始扫描的 IP 网段列表（会自动拆分为 /24 子网进行测试）。
*   `bind_addr`: 本地 TCP 转发服务的监听地址。
*   `web_addr`: 本地 Web 监控服务的监听地址。
*   `trace_url`: 用于测试 IP 延迟和获取 Colo 信息的 URL。
*   `asn_url`: 用于动态更新 CIDR 列表的 URL。
*   `ip_store_file`: 扫描结果的保存路径。
*   **策略参数**：
    *   `selection_top_k_percent`: 仅使用评分最高的前 `k`% 的 IP 段进行转发（例如 0.1 表示前 10%）。
    *   `selection_random_n_subnets`: 每次建立连接时，从优选 IP 段中随机选取的 IP 段数量。
    *   `selection_random_m_ips`: 在每个选中的 IP 段内，随机生成的 IP 数量。
    *   *说明：总并发连接尝试数 = n * m*
    *   `staggered_delay_ms`: Happy Eyeballs 竞速连接的交错延迟（毫秒），范围 10-2000（默认 200）。优先给高质量 IP 握手时间，减少不必要的并发 SYN 包。
*   **扫描策略 (`scan_strategy`)**:
    *   `type`: 扫描策略类型，可选 `"adaptive"` (自适应，推荐) 或 `"full_scan"` (全量扫描)。
    *   `initial_scan_mask`: [自适应] 阶段一（广域稀疏扫描）使用的 CIDR 掩码（默认 20）。
    *   `initial_samples_per_subnet`: [自适应] 阶段一每个大网段的采样数（默认 1）。
    *   `promising_subnet_percent`: [自适应] 阶段二筛选“热点区域”的比例（默认 0.2，即前 20%）。
    *   `focused_samples_per_subnet`: [自适应] 阶段三（精细扫描）每个 /24 子网的采样数（默认 3）。
*   **后台扫描 (`background_scan`)**:
    *   `enabled`: 是否启用后台定时扫描（默认 false）。
    *   `interval_secs`: 两次扫描的间隔时间，单位秒（默认 3600，最小 60）。
    *   `concurrency`: 后台扫描时的并发数（默认 100）。
*   **预连接池 (`connection_pool`)**:
    *   `enabled`: 是否启用预连接池（默认 false）。
    *   `pool_size`: 池容量上限（默认 8）。
    *   `min_idle`: 后台定时维持的最小空闲连接数（默认 2，不得超过 pool_size）。
    *   `max_idle_secs`: 连接最大空闲时间，单位秒（默认 12，最小 5）。Cloudflare 约 15 秒关闭空闲连接，建议设为 12 以留余量。
    *   `refill_interval_ms`: 后台定时清理和水位检查的间隔，单位毫秒（默认 5000，最小 100）。
*   **TCP Keepalive (`tcp_keepalive`)**:
    *   `time_secs`: Keepalive 空闲时间（秒）（默认 60）。
    *   `interval_secs`: Keepalive 探测间隔（秒）（默认 10）。
*   **评分权重 (`scoring`)**:
    *   `base_score`: 基础分数（默认 100，必须 > 0）。
    *   `latency_penalty_per_10ms`: 延迟惩罚系数，每 10ms 扣分（默认 1.0）。
    *   `jitter_penalty_per_5ms`: 抖动惩罚系数，每 5ms 扣分（默认 1.0）。
    *   `loss_penalty_per_percent`: 丢包惩罚系数，每 1% 丢包扣分（默认 50.0）。
    *   *公式：评分 = base_score - latency/10×系数 - jitter/5×系数 - loss%×系数*

**注意**：配置文件支持 JSONC 格式（允许 `//` 和 `/* */` 注释）。所有配置参数均会自动验证，不合法时程序拒绝启动并显示错误消息。

### 3. 使用步骤

本项目分为**扫描**和**转发**两个阶段。

#### 阶段一：扫描优选 IP 段

在使用转发功能前，需要先运行扫描模式来获取高质量 IP 段。扫描完成后，程序会自动退出并将结果保存到 `ip_store_file`。

```bash
# 开发模式
cargo run -- --scan

# 生产模式
./target/release/tcp-forwarder-rust --scan
```

#### 阶段二：启动转发服务

扫描完成后，直接运行程序即可启动转发服务。程序会加载之前的扫描结果，并根据配置的策略开始监听端口和转发流量。

```bash
# 开发模式
cargo run
# 或启用后台扫描和立即扫描
cargo run -- --scan-on-start

# 生产模式
./target/release/tcp-forwarder-rust
# 或
./target/release/tcp-forwarder-rust --scan-on-start
```

#### 其他工具命令

查看数据中心质量排名（基于已有的扫描结果）：

```bash
cargo run -- --rank-colos
# 或
./target/release/tcp-forwarder-rust --rank-colos
```

此命令将按平均评分降序输出各数据中心的统计信息，帮助您决定 `config.jsonc` 中 `target_colos` 的设置。

### 4. 构建生产版本

为了获得最佳性能，建议使用 release 模式构建：

```bash
cargo build --release
```

构建产物位于 `target/release/tcp-forwarder-rust` (Linux/macOS) 或 `target/release/tcp-forwarder-rust.exe` (Windows)。

## 最近改进

### v0.2.8 (2026-02-21)

**核心改进：**

1.  **可靠性与正确性修复**:
    - **Accept 循环非阻塞化**：将 FD 信号量获取从 accept 循环移入 `tokio::spawn` 内部，防止信号量耗尽时整个 accept 循环被挂起、无法接收新连接。
    - **消除假 async 方法**：移除 `pool.rs` 中 `acquire`/`snapshot`/`push` 等方法上无意义的 `async` 标记，消除误导性 API。
    - **连接池单锁 purge**：将 `purge_stale()` 从双次加锁改为单次加锁内 retain，消除两次加锁间的 cache miss 窗口。

2.  **扫描器模块化重构**:
    - 将原 `scanner.rs`（~981 行）拆分为 `scanner/mod.rs`、`scanner/executor.rs`、`scanner/probe.rs`、`scanner/asn.rs` 四个子模块，严格遵守文件不超过 1000 行的规范。

3.  **转发指标与可观测性**:
    - 新增 `metrics.rs` 模块，基于原子操作实现无锁转发统计（活跃连接数、竞速成功率、吞吐字节数等）。
    - 新增 `/api/v1/stats` 端点，实时查看转发服务运行指标。
    - 新增 `/debug` 端点，输出调试级别的内部状态。

4.  **评分权重可配置化**:
    - 新增 `scoring` 配置块，支持自定义基础分、延迟/抖动/丢包惩罚系数，适应不同网络场景对质量指标的侧重需求。
    - 配置参数带完整验证（`base_score > 0`，惩罚系数 `>= 0`）。

5.  **代码规范与质量**:
    - 消除 `pool.rs` 中残留的 `else` 分支，统一使用卫语句模式。
    - `state.rs` 中 `filter_subnets_by_colo` 结果缓存化，避免高频转发场景下重复构建 `HashSet`。
    - 修复 `main.rs` 中 `unwrap_or(())` 吞掉 `spawn_blocking` panic 信息的问题。

### v0.2.7 (2026-02-20)

**核心改进：**

1.  **架构与并发性能优化**:
    - **锁性能优化**：移除了预连接池中低效且不合理的异步锁 `tokio::sync::Mutex`，替换为无锁等待的高性能同步锁 `parking_lot::Mutex`，消除了协程调度开销。
    - **指数退避重试 (Exponential Backoff)**：连接池补充失败时引入退避机制（最高退避至 30 秒），防止 Cloudflare 节点宕机时导致的 CPU 打满和句柄耗尽。
    - **消除异步阻塞 I/O**：将扫描结果持久化等磁盘 I/O 操作转移到 `tokio::task::spawn_blocking` 中执行，避免阻塞 Tokio Reactor 线程。
    - **TCP Keepalive 可配置化**：将探活时间与间隔提取到全局配置 `tcp_keepalive` 中，适应不同网络 NAT 老化环境。

2.  **严格的代码规范审计**:
    - **消除反模式**：彻底重构了为规避 `else` 导致的 `.unwrap()` 滥用，采用安全的前置卫语句 (`match` 提前返回) 模式。
    - **大函数与复杂度拆分**：根据“单一职责”原则，重构拆分了原先超过 60 行阈值的大函数（如 `AppConfig::validate` 等）。
    - 修正了部分行宽超过 80 字符的长链式调用。

### v0.2.6 (2026-02-19)

**核心改进：**

1.  **阶梯延迟连接竞速 (Staggered-start Racing)**:
    - 引入 `staggered_delay_ms` 参数，实现类 Happy Eyeballs 的阶梯启动机制。
    - 优先给高质量 IP 握手时间，减少不必要的并发 SYN 包，降低系统资源开销。

2.  **连接池深度优化**:
    - 使用 `FuturesUnordered` 实现连接池的并行异步补充，大幅提升水位恢复速度。
    - 优化 LIFO 管理逻辑，采用更细粒度的锁竞争优化，减少高并发下的互斥开销。

3.  **扫描器性能跃升**:
    - 引入基于 `Rayon` 的并行统计分析，利用多核性能加速数据中心评分计算。
    - 采用延迟加载（Lazy Async Stream）技术优化扫描过程中的内存占用。
    - 优化子网拆分与交叉采样算法，扫描效率提升约 40%。

4.  **架构审计与代码规范**:
    - 全面重构核心逻辑，严格执行“禁止 else”、嵌套层级限制及函数规模控制。
    - 增加 `max_open_files` 配置，支持动态调整系统 FD 信号量。

### v0.2.5 (2026-02-16)

**核心改进：**

1.  **预连接池 (Connection Pool)**:
    - 后台预建立 TCP 连接并维持最小空闲水位，客户端请求时直接复用，消除 TCP 握手延迟。
    - 池空时自动降级为原有的并发竞速连接模式，零风险启用。
    - 支持响应式补充（acquire 后立即触发）和定时清理（过期/死连接自动淘汰）。
    - 新增 Web API `/api/v1/pool` 查看连接池统计（命中率、过期数、淘汰数等）。

2.  **JSONC 配置格式**:
    - 配置文件从 `config.json` 迁移为 `config.jsonc`，支持 `//` 和 `/* */` 注释。
    - 引入 `json_comments` 库进行注释剥离，对现有配置完全兼容。

### v0.2.4 (2026-02-15)

**核心改进：**

1.  **后台定时扫描 (Background Scanning)**:
    - 支持在转发服务运行时，后台自动执行 IP 扫描任务。
    - 扫描结果会自动持久化并实时更新 IP 池。
    - 解决了长期运行后 IP 质量下降的问题。

2.  **启动自检**:
    - 新增 `--scan-on-start` 参数，支持启动时立即执行一次全量/自适应扫描。
    - 即使配置文件中禁用了后台扫描，使用此参数也会强制开启。

### v0.2.3 (2026-01-10)

**核心改进：**

1.  **自适应扫描策略 (Adaptive Scanning)**:
    - 引入了三阶段扫描机制：广域稀疏扫描 -> 热点区域分析 -> 重点区域精细扫描。
    - 相比传统的全量扫描，大幅减少了无效探测，显著提升了扫描效率。
    - 支持通过配置文件灵活调整各阶段的参数（掩码大小、采样数、筛选比例等）。

2.  **配置系统升级**:
    - 新增 `scan_strategy` 配置块，支持动态切换扫描策略。
    - 完善了配置参数的验证逻辑，确保自适应扫描参数的合法性。

### v0.2.2 (2026-01-04)

**核心改进：**

1. **性能与体验优化**：
   - **扫描进度可视化**: 引入 `indicatif` 库，扫描时显示美观的进度条、预计剩余时间和实时结果统计，告别盲目等待。
   - **极致连接竞速**: 升级连接引擎，使用 `futures::stream::FuturesUnordered` 替代传统的任务生成模式，在高并发场景下显著减少任务调度开销，连接建立更极速、更稳定。

2. **核心改进（v0.2.1）**：
   - **优雅关闭机制**：使用 `CancellationToken` 实现，确保服务平滑退出。
   - **并发连接竞速优化**：修复顺序等待问题，实现真正的并行连接。
   - **类型安全改进**：使用 `thiserror` 和更合理的数值类型。
   - **新增 API**：`/health` 健康检查和 `/api/v1/subnets` 元数据接口。

**升级指南**：
- 直接重新编译即可：`cargo build --release`
- 配置文件和数据格式保持兼容。

### v0.2.0 (2026-01-03)

**核心改进：**

1. **错误处理增强**：
   - 改进配置文件加载失败时的错误日志
   - 添加配置参数自动验证，防止无效参数导致运行时错误

2. **性能优化**：
   - 使用 `tokio::select!` 替代忙等待模式，提高并发连接选择效率
   - 优化内存管理，预分配容器容量，减少重新分配开销
   - 使用 `Arc` 共享配置数据，减少不必要的克隆操作

3. **可观测性提升**：
   - 添加详细的调试日志，记录 IP 评分计算过程
   - 改进错误日志级别，更清晰地区分信息、警告和错误

4. **网络改进**：
   - 添加单独的连接超时（1秒），区分于总体请求超时（2秒）
   - 更细粒度的超时控制，提高网络操作响应性

5. **代码质量**：
   - 重构分析功能到独立模块 `src/analytics.rs`
   - 改进代码结构和模块化设计
   - 添加更完整的错误处理和资源清理

## 监控接口

服务启动后，可以访问 Web 接口查看服务状态：

*   **IP 段状态**: `http://localhost:3000/status` (默认)
*   **健康检查**: `http://localhost:3000/health`
*   **子网列表 (带元数据)**: `http://localhost:3000/api/v1/subnets`
*   **连接池统计**: `http://localhost:3000/api/v1/pool`（未启用时返回 404）
*   **转发统计**: `http://localhost:3000/api/v1/stats`
*   **调试信息**: `http://localhost:3000/debug`

### IP 段状态示例

返回格式: JSON 数组，按评分从高到低排序。

```json
[
  {
    "subnet": "1.2.3.0/24",
    "score": 85.5,
    "avg_latency": 120,
    "avg_jitter": 5,
    "avg_loss_rate": 0.0,
    "sample_count": 3,
    "colo": "LAX",
    "last_updated": "2023-10-27T10:00:00Z"
  },
  ...
]
```

### 连接池统计示例

```json
{
  "pool_size": 5,
  "total_acquired": 128,
  "hits": 110,
  "misses": 18,
  "expired": 7,
  "dead": 3,
  "evicted": 0,
  "cleaned": 10
}
```

### 转发统计示例

```json
{
  "total_connections": 1024,
  "active_connections": 5,
  "race_successes": 980,
  "race_failures": 44,
  "pool_hits": 512,
  "bytes_tx": 10485760,
  "bytes_rx": 52428800
}
```
