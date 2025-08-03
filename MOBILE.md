# 📱 TCP Forwarder 移动平台支持

## 🎯 概述

TCP Forwarder 现已全面支持移动平台，包括 Android 和 iOS 设备。这使得您可以在手机、平板等移动设备上运行高性能的 TCP 转发服务。

## 🚀 支持的平台

### Android 平台
| 架构 | 目标三元组 | 设备类型 | 说明 |
|------|------------|----------|------|
| ARM64 | `aarch64-linux-android` | 现代Android设备 | 主流设备架构，性能最佳 |
| ARMv7 | `armv7-linux-androideabi` | 较老Android设备 | 兼容性广，覆盖老设备 |
| x86 | `i686-linux-android` | Android模拟器 | 用于开发和测试 |
| x86_64 | `x86_64-linux-android` | Android模拟器/设备 | 高性能模拟器和部分设备 |

### iOS 平台
| 架构 | 目标三元组 | 设备类型 | 说明 |
|------|------------|----------|------|
| ARM64 | `aarch64-apple-ios` | iPhone/iPad | 真实设备运行 |
| x86_64 | `x86_64-apple-ios` | iOS模拟器 | Intel Mac模拟器 |
| ARM64 Sim | `aarch64-apple-ios-sim` | iOS模拟器 | Apple Silicon Mac模拟器 |

## 📦 获取移动版本

### 从 GitHub Releases 下载
```bash
# 下载最新发布版本的移动平台包
wget https://github.com/GlenPrts/TCP-Forwarder-Rust/releases/latest/download/tcp-forwarder-mobile-package.tar.gz

# 解压
tar -xzf tcp-forwarder-mobile-package.tar.gz
cd mobile-package/
```

### 自行编译
```bash
# Android (需要安装 Android NDK)
cargo build --release --target aarch64-linux-android

# iOS (需要 macOS 和 Xcode)
cargo build --release --target aarch64-apple-ios
```

## 🔧 Android 部署指南

### 1. 通过 ADB 部署
```bash
# 推送二进制文件到设备
adb push android/tcp-forwarder-aarch64-linux-android /data/local/tmp/tcp-forwarder
adb shell chmod +x /data/local/tmp/tcp-forwarder

# 推送配置文件
adb push config.yaml /sdcard/tcp-forwarder/config.yaml

# 运行程序
adb shell '/data/local/tmp/tcp-forwarder --config /sdcard/tcp-forwarder/config.yaml'
```

### 2. 终端应用部署
使用支持的终端应用（如 Termux）：
```bash
# 在 Termux 中
cp tcp-forwarder-aarch64-linux-android $PREFIX/bin/tcp-forwarder
chmod +x $PREFIX/bin/tcp-forwarder

# 运行
tcp-forwarder --config ~/tcp-forwarder/config.yaml
```

### 3. Android 应用集成
```java
// 在 Android 应用中调用
Runtime.getRuntime().exec("/data/data/your.package/files/tcp-forwarder --config /data/data/your.package/files/config.yaml");
```

## 🍎 iOS 部署指南

### 1. 越狱设备部署
```bash
# 通过 SSH 上传
scp ios/tcp-forwarder-aarch64-apple-ios root@device:/usr/local/bin/tcp-forwarder
ssh root@device 'chmod +x /usr/local/bin/tcp-forwarder'

# 运行
ssh root@device '/usr/local/bin/tcp-forwarder --config /var/mobile/config.yaml'
```

### 2. iOS 应用集成
需要创建 Objective-C/Swift 绑定来在 iOS 应用中使用。

## ⚙️ 移动平台配置

### 基础配置示例
```yaml
proxy:
  dial_timeout: 10s
  idle_timeout: 60s

pools:
  mobile_pool:
    health_check: true
    health_check_interval: 30s
    max_retries: 3
    weight: 1.0
    endpoints:
      - "your-server.com:8080"

server:
  bind: "127.0.0.1:3000"  # 本地绑定

# 移动平台建议禁用或简化指标收集
metrics:
  enabled: false

scoring:
  algorithm: "ewma"
  ewma:
    alpha: 0.1
    max_score: 1000.0
    min_score: 1.0
    window_size: 5  # 减少内存使用
```

### Android 特定配置
```yaml
# Android 配置建议
proxy:
  dial_timeout: 15s    # 移动网络延迟较高
  idle_timeout: 120s   # 保持长连接

server:
  bind: "0.0.0.0:8080" # 监听所有接口

# 日志配置
logging:
  level: "info"
  file: "/sdcard/tcp-forwarder/logs/app.log"
```

### iOS 特定配置
```yaml
# iOS 配置建议
proxy:
  dial_timeout: 20s    # iOS 网络限制较严格
  idle_timeout: 300s   # 长连接保持

server:
  bind: "127.0.0.1:3000" # 仅本地访问

# 沙盒兼容路径
logging:
  file: "Documents/tcp-forwarder/app.log"
```

## 🔒 安全注意事项

### Android 安全
- **权限要求**: 需要网络权限
- **SELinux**: 某些功能可能受到 SELinux 限制
- **防火墙**: 可能需要配置防火墙规则
- **存储**: 配置文件放在 `/sdcard` 可被其他应用读取

### iOS 安全
- **沙盒限制**: 应用沙盒限制文件访问
- **网络权限**: 需要在 Info.plist 中声明
- **后台运行**: 受 iOS 后台任务限制
- **证书**: 可能需要开发者证书

## 📊 性能优化

### 移动平台优化建议
1. **内存使用**: 减少缓存大小和连接池数量
2. **电池优化**: 调整健康检查间隔和超时时间
3. **网络适应**: 考虑移动网络的不稳定性
4. **功能精简**: 禁用不必要的功能（如指标收集）

### 配置示例
```yaml
# 移动设备优化配置
pools:
  default:
    health_check_interval: 60s  # 减少检查频率
    max_connections: 10         # 限制连接数
    connection_timeout: 30s     # 适应移动网络

scoring:
  ewma:
    window_size: 3              # 减少内存使用

# 禁用资源密集型功能
metrics:
  enabled: false
```

## 🐛 故障排除

### 常见问题

#### Android
1. **权限被拒绝**
   ```bash
   # 检查 SELinux 状态
   adb shell getenforce
   
   # 临时设置为宽松模式 (需要 root)
   adb shell su -c "setenforce 0"
   ```

2. **网络连接失败**
   ```bash
   # 检查网络权限
   adb shell dumpsys package your.package.name | grep permission
   ```

3. **文件路径问题**
   ```bash
   # 使用 Android 兼容路径
   /data/local/tmp/        # 临时文件
   /sdcard/                # 外部存储
   /data/data/pkg/files/   # 应用私有目录
   ```

#### iOS
1. **应用崩溃**
   - 检查内存使用和沙盒权限
   - 查看系统日志: 设置 → 隐私与安全 → 分析与改进

2. **网络权限**
   - 确保 Info.plist 包含网络使用说明
   - 检查网络安全策略

3. **后台运行限制**
   - iOS 限制后台网络活动
   - 考虑使用前台服务或推送通知

## 🔄 更新和维护

### 自动更新
移动平台可以实现自动更新机制：

```bash
# Android 更新脚本
#!/bin/bash
LATEST_URL="https://api.github.com/repos/GlenPrts/TCP-Forwarder-Rust/releases/latest"
DOWNLOAD_URL=$(curl -s $LATEST_URL | grep "tcp-forwarder-android-aarch64" | cut -d'"' -f4)
wget $DOWNLOAD_URL -O /data/local/tmp/tcp-forwarder-new
mv /data/local/tmp/tcp-forwarder-new /data/local/tmp/tcp-forwarder
chmod +x /data/local/tmp/tcp-forwarder
```

### 配置同步
可以使用云存储服务同步配置：

```yaml
# 支持云配置
config_source: "https://your-server.com/mobile-config.yaml"
config_update_interval: "24h"
```

## 📚 开发资源

### 交叉编译环境
```bash
# 安装 Android 目标
rustup target add aarch64-linux-android
rustup target add armv7-linux-androideabi
rustup target add i686-linux-android
rustup target add x86_64-linux-android

# 安装 iOS 目标 (仅 macOS)
rustup target add aarch64-apple-ios
rustup target add x86_64-apple-ios
rustup target add aarch64-apple-ios-sim
```

### 构建脚本
```bash
#!/bin/bash
# 移动平台构建脚本

echo "构建 Android 版本..."
for target in aarch64-linux-android armv7-linux-androideabi x86_64-linux-android; do
    echo "构建 $target..."
    cargo build --release --target $target --features mobile
done

if [[ "$OSTYPE" == "darwin"* ]]; then
    echo "构建 iOS 版本..."
    for target in aarch64-apple-ios x86_64-apple-ios; do
        echo "构建 $target..."
        cargo build --release --target $target --features mobile
    done
fi
```

## 🤝 贡献指南

### 移动平台测试
- Android: 使用 Android Studio AVD 或真实设备
- iOS: 使用 Xcode Simulator 或真实设备
- 跨平台: 确保功能在所有目标平台正常工作

### 提交要求
- 测试所有移动平台目标
- 更新相关文档
- 遵循移动平台最佳实践

---

移动平台支持让 TCP Forwarder 的应用场景更加广泛，从桌面服务器扩展到移动设备，实现真正的跨平台 TCP 转发解决方案。
