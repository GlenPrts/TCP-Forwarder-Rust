# TCP Forwarder Rust

这是一个高性能的 TCP 转发器，旨在通过优选 IP（如 Cloudflare）转发流量。

## 快速开始

### 1. 环境要求

确保你已经安装了 Rust 编程语言环境。如果没有，请访问 [rustup.rs](https://rustup.rs/) 安装。

### 2. 配置文件

项目根目录下有一个 `config.json` 文件。在启动前，你可以根据需要编辑它：

```json
{
  "target_port": 443,              // 目标端口
  "target_colos": ["SJC", "LAX"],  // 目标数据中心代码（用于筛选 IP）
  "cidr_list": [...],              // 初始扫描的 CIDR 列表
  "bind_addr": "0.0.0.0:8080",     // TCP 转发服务监听地址
  "web_addr": "0.0.0.0:3000",      // Web 状态监控服务监听地址
  "max_warm_connections": 100,     // 最大热连接数
  "trace_url": "...",              // 用于测试延迟的 URL
  "asn_url": "..."                 // 用于获取 CIDR 列表的 URL
}
```

### 3. 编译与运行

**开发模式运行：**

直接使用 Cargo 运行：

```bash
cargo run
```

**生产模式构建与运行：**

为了获得最佳性能，建议使用 release 模式构建：

```bash
# 1. 构建
cargo build --release

# 2. 运行 (Linux/macOS)
./target/release/tcp-forwarder-rust

# 或者 (Windows)
.\target\release\tcp-forwarder-rust.exe
```

## 功能说明

*   **自动优选**: 自动扫描并维护一个高质量 IP 池。
*   **热连接池**: 预先建立连接，降低用户连接时的延迟。
*   **Web 监控**: 访问 `http://localhost:3000/status` (默认) 查看当前 IP 池状态。
*   **故障转移**: 如果优选 IP 不可用，会自动降级或重试。
