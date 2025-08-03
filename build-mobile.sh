#!/bin/bash

# TCP Forwarder 移动平台构建脚本
# 用于本地构建 Android 和 iOS 版本

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 打印带颜色的消息
print_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 检查命令是否存在
check_command() {
    if ! command -v "$1" &> /dev/null; then
        print_error "$1 命令未找到，请先安装"
        return 1
    fi
}

# 检查基本工具
print_info "检查构建环境..."
check_command "cargo" || exit 1
check_command "rustc" || exit 1

# 切换到项目目录
cd "$(dirname "$0")/tcp-forwarder"

# Android 构建
build_android() {
    print_info "开始构建 Android 版本..."
    
    # 检查 Android NDK
    if [ -z "$ANDROID_NDK_HOME" ]; then
        print_warning "ANDROID_NDK_HOME 未设置，尝试自动检测..."
        
        # 常见的 NDK 路径
        NDK_PATHS=(
            "$HOME/Android/Sdk/ndk-bundle"
            "$HOME/Library/Android/sdk/ndk-bundle"
            "/opt/android-ndk"
            "/usr/local/android-ndk"
        )
        
        for path in "${NDK_PATHS[@]}"; do
            if [ -d "$path" ]; then
                export ANDROID_NDK_HOME="$path"
                print_info "找到 Android NDK: $path"
                break
            fi
        done
        
        if [ -z "$ANDROID_NDK_HOME" ]; then
            print_error "未找到 Android NDK，请设置 ANDROID_NDK_HOME 环境变量"
            return 1
        fi
    fi
    
    # Android 目标列表
    ANDROID_TARGETS=(
        "aarch64-linux-android"
        "armv7-linux-androideabi" 
        "i686-linux-android"
        "x86_64-linux-android"
    )
    
    for target in "${ANDROID_TARGETS[@]}"; do
        print_info "安装目标: $target"
        rustup target add "$target" || {
            print_error "安装目标 $target 失败"
            continue
        }
        
        print_info "构建 $target..."
        
        # 设置环境变量
        case "$target" in
            aarch64-linux-android)
                export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang"
                ;;
            armv7-linux-androideabi)
                export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi21-clang"
                ;;
            i686-linux-android)
                export CARGO_TARGET_I686_LINUX_ANDROID_LINKER="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/i686-linux-android21-clang"
                ;;
            x86_64-linux-android)
                export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android21-clang"
                ;;
        esac
        
        # 构建
        if cargo build --release --target "$target" --features mobile; then
            print_success "✅ $target 构建成功"
            
            # 检查文件大小
            binary_path="target/$target/release/tcp-forwarder"
            if [ -f "$binary_path" ]; then
                size=$(du -h "$binary_path" | cut -f1)
                print_info "二进制文件大小: $size"
            fi
        else
            print_error "❌ $target 构建失败"
        fi
    done
    
    print_success "Android 构建完成！"
}

# iOS 构建 (仅限 macOS)
build_ios() {
    if [[ "$OSTYPE" != "darwin"* ]]; then
        print_warning "iOS 构建仅支持 macOS 系统"
        return 0
    fi
    
    print_info "开始构建 iOS 版本..."
    
    # 检查 Xcode 命令行工具
    if ! xcode-select -p &> /dev/null; then
        print_error "Xcode 命令行工具未安装，请运行: xcode-select --install"
        return 1
    fi
    
    # iOS 目标列表
    IOS_TARGETS=(
        "aarch64-apple-ios"
        "x86_64-apple-ios"
        "aarch64-apple-ios-sim"
    )
    
    for target in "${IOS_TARGETS[@]}"; do
        print_info "安装目标: $target"
        rustup target add "$target" || {
            print_error "安装目标 $target 失败"
            continue
        }
        
        print_info "构建 $target..."
        
        # 设置 iOS 部署目标
        export IPHONEOS_DEPLOYMENT_TARGET=12.0
        
        # 构建
        if cargo build --release --target "$target" --features mobile; then
            print_success "✅ $target 构建成功"
            
            # 检查文件大小
            binary_path="target/$target/release/tcp-forwarder"
            if [ -f "$binary_path" ]; then
                size=$(du -h "$binary_path" | cut -f1)
                print_info "二进制文件大小: $size"
            fi
        else
            print_error "❌ $target 构建失败"
        fi
    done
    
    print_success "iOS 构建完成！"
}

# 创建移动平台包
create_mobile_package() {
    print_info "创建移动平台包..."
    
    mkdir -p ../mobile-dist/{android,ios,docs}
    
    # 复制 Android 二进制文件
    for target in aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android; do
        if [ -f "target/$target/release/tcp-forwarder" ]; then
            arch=$(echo "$target" | cut -d'-' -f1)
            cp "target/$target/release/tcp-forwarder" "../mobile-dist/android/tcp-forwarder-$arch"
            print_success "复制 Android $arch 二进制文件"
        fi
    done
    
    # 复制 iOS 二进制文件 (如果存在)
    if [[ "$OSTYPE" == "darwin"* ]]; then
        for target in aarch64-apple-ios x86_64-apple-ios aarch64-apple-ios-sim; do
            if [ -f "target/$target/release/tcp-forwarder" ]; then
                arch=$(echo "$target" | cut -d'-' -f1)
                suffix=""
                if [[ "$target" == *"sim" ]]; then
                    suffix="-sim"
                fi
                cp "target/$target/release/tcp-forwarder" "../mobile-dist/ios/tcp-forwarder-$arch$suffix"
                print_success "复制 iOS $arch$suffix 二进制文件"
            fi
        done
    fi
    
    # 复制文档
    cp ../MOBILE.md ../mobile-dist/docs/
    cp ../config.yaml ../mobile-dist/
    
    # 创建使用说明
    cat > ../mobile-dist/README.md << 'EOF'
# TCP Forwarder 移动平台包

此包包含了为移动平台预编译的 TCP Forwarder 二进制文件。

## 目录结构

- `android/` - Android 平台二进制文件
- `ios/` - iOS 平台二进制文件
- `docs/` - 详细文档
- `config.yaml` - 示例配置文件

## 快速开始

### Android
```bash
# 选择适合您设备的架构
adb push android/tcp-forwarder-aarch64 /data/local/tmp/tcp-forwarder
adb shell chmod +x /data/local/tmp/tcp-forwarder
adb push config.yaml /sdcard/tcp-forwarder/
adb shell '/data/local/tmp/tcp-forwarder --config /sdcard/tcp-forwarder/config.yaml'
```

### iOS
```bash
# 越狱设备
scp ios/tcp-forwarder-aarch64 root@device:/usr/local/bin/tcp-forwarder
scp config.yaml root@device:/var/mobile/
ssh root@device '/usr/local/bin/tcp-forwarder --config /var/mobile/config.yaml'
```

详细说明请参阅 `docs/MOBILE.md`。
EOF
    
    # 创建压缩包
    cd ../mobile-dist
    tar -czf tcp-forwarder-mobile-$(date +%Y%m%d).tar.gz *
    cd ../tcp-forwarder
    
    print_success "移动平台包已创建: ../mobile-dist/"
}

# 清理构建文件
clean() {
    print_info "清理构建文件..."
    cargo clean
    rm -rf ../mobile-dist
    print_success "清理完成"
}

# 显示帮助信息
show_help() {
    echo "TCP Forwarder 移动平台构建脚本"
    echo ""
    echo "用法: $0 [选项]"
    echo ""
    echo "选项:"
    echo "  android     构建 Android 版本"
    echo "  ios         构建 iOS 版本 (仅限 macOS)"
    echo "  all         构建所有支持的移动平台"
    echo "  package     创建移动平台分发包"
    echo "  clean       清理构建文件"
    echo "  help        显示此帮助信息"
    echo ""
    echo "环境变量:"
    echo "  ANDROID_NDK_HOME    Android NDK 安装路径"
    echo ""
    echo "示例:"
    echo "  $0 all              # 构建所有平台"
    echo "  $0 android          # 仅构建 Android"
    echo "  $0 package          # 创建分发包"
}

# 主函数
main() {
    case "${1:-all}" in
        android)
            build_android
            ;;
        ios)
            build_ios
            ;;
        all)
            build_android
            build_ios
            ;;
        package)
            build_android
            build_ios
            create_mobile_package
            ;;
        clean)
            clean
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            print_error "未知选项: $1"
            show_help
            exit 1
            ;;
    esac
}

# 运行主函数
main "$@"
