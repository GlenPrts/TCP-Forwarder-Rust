# TCP Forwarder Rust

这是一个高性能的 TCP 转发器，旨在通过优选 IP（如 Cloudflare）转发流量。它采用扫描与转发分离的设计，结合并发连接竞速机制，提供低延迟、高可靠的 TCP 转发服务。

## 核心特性

*   **独立扫描模式**: 专门的扫描模式 (`--scan`)，支持从 ASN 动态获取 CIDR，自动筛选高分 IP。
*   **持久化存储**: 扫描结果保存到本地文件，转发服务启动时直接加载，无需等待扫描。
*   **并发连接竞速**: 转发流量时，同时向多个优选 IP (默认 Top 5) 发起连接，使用最快建立的连接，显著降低握手延迟。
*   **Web 状态监控**: 内置 Web 服务器，实时查看当前 IP 池状态和质量评分。
*   **故障自动转移**: 智能处理连接失败，自动尝试备用 IP。

## 快速开始

### 1. 环境准备

确保已安装 Rust 编程语言环境。如果没有，请访问 [rustup.rs](https://rustup.rs/) 安装。

### 2. 配置文件

在项目根目录下创建或修改 `config.json` 文件：

```json
{
  "target_port": 443,
  "target_colos": [
    "SJC",
    "LAX"
  ],
  "cidr_list": [
    "104.16.0.0/12",
    "172.64.0.0/13"
  ],
  "bind_addr": "0.0.0.0:8080",
  "web_addr": "0.0.0.0:3000",
  "trace_url": "http://engage.cloudflareclient.com/cdn-cgi/trace",
  "asn_url": "https://asn.0x01111110.com/13335?4",
  "ip_store_file": "ip_results.json"
}
```

**配置项说明：**

*   `target_port`: 目标服务的端口（例如 Cloudflare 节点的 443 端口）。
*   `target_colos`: 期望的数据中心代码列表（如 "SJC", "LAX"）。如果留空 `[]`，则不限制数据中心。
*   `cidr_list`: 初始扫描的 IP 网段列表。
*   `bind_addr`: 本地 TCP 转发服务的监听地址。
*   `web_addr`: 本地 Web 监控服务的监听地址。
*   `trace_url`: 用于测试 IP 延迟和获取 Colo 信息的 URL。
*   `asn_url`: 用于动态更新 CIDR 列表的 URL。
*   `ip_store_file`: 扫描结果的保存路径。

### 3. 使用步骤

本项目分为**扫描**和**转发**两个阶段。

#### 阶段一：扫描优选 IP

在使用转发功能前，需要先运行扫描模式来获取高质量 IP。扫描完成后，程序会自动退出并将结果保存到 `ip_store_file`。

```bash
# 开发模式
cargo run -- --scan

# 生产模式
./target/release/tcp-forwarder-rust --scan
```

#### 阶段二：启动转发服务

扫描完成后，直接运行程序即可启动转发服务。程序会加载之前的扫描结果，并开始监听端口。

```bash
# 开发模式
cargo run

# 生产模式
./target/release/tcp-forwarder-rust
```

### 4. 构建生产版本

为了获得最佳性能，建议使用 release 模式构建：

```bash
cargo build --release
```

构建产物位于 `target/release/tcp-forwarder-rust` (Linux/macOS) 或 `target/release/tcp-forwarder-rust.exe` (Windows)。

## 监控接口

服务启动后，可以访问 Web 接口查看当前加载的优选 IP 列表及其质量评分：

*   地址: `http://localhost:3000/status` (默认)
*   返回格式: JSON 数组，按评分从高到低排序。

```json
[
  {
    "ip": "1.2.3.4",
    "latency": 120,
    "jitter": 5,
    "loss_rate": 0.0,
    "score": 85.5,
    "colo": "LAX",
    "last_updated": "2023-10-27T10:00:00Z"
  },
  ...
]
