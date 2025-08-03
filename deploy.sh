#!/bin/bash

# =============================================================================
# Rust TCP智能转发器 - Docker构建和部署脚本
# =============================================================================

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 打印带颜色的消息
print_message() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

print_step() {
    echo -e "${BLUE}[STEP]${NC} $1"
}

# 检查必要的文件
check_prerequisites() {
    print_step "检查必要文件..."
    
    if [ ! -f "tcp-forwarder/Cargo.toml" ]; then
        print_error "Cargo.toml文件不存在"
        exit 1
    fi
    
    if [ ! -f "tcp-forwarder/config.yaml" ]; then
        print_error "config.yaml文件不存在"
        exit 1
    fi
    
    if [ ! -f "tcp-forwarder/ip_list.txt" ]; then
        print_warning "ip_list.txt文件不存在，将创建示例文件"
        cat > tcp-forwarder/ip_list.txt << EOF
# TCP转发目标IP列表
# 每行一个IP地址
8.8.8.8
8.8.4.4
1.1.1.1
1.0.0.1
EOF
    fi
    
    print_message "必要文件检查完成"
}

# 构建Docker镜像
build_image() {
    print_step "构建Docker镜像..."
    
    IMAGE_TAG=${1:-"tcp-forwarder:latest"}
    
    docker build -t "$IMAGE_TAG" .
    
    if [ $? -eq 0 ]; then
        print_message "Docker镜像构建成功: $IMAGE_TAG"
    else
        print_error "Docker镜像构建失败"
        exit 1
    fi
}

# 运行容器（简单模式）
run_simple() {
    print_step "启动TCP转发器（简单模式）..."
    
    # 停止现有容器
    docker stop tcp-forwarder 2>/dev/null || true
    docker rm tcp-forwarder 2>/dev/null || true
    
    # 创建日志目录
    mkdir -p logs
    
    # 启动新容器
    docker run -d \
        --name tcp-forwarder \
        --restart unless-stopped \
        -p 1234:1234 \
        -p 9099:9099 \
        -v "$(pwd)/tcp-forwarder/config.yaml:/app/config/config.yaml:ro" \
        -v "$(pwd)/tcp-forwarder/ip_list.txt:/app/config/ip_list.txt:ro" \
        -v "$(pwd)/logs:/app/logs" \
        -e RUST_LOG=info \
        tcp-forwarder:latest
    
    print_message "TCP转发器已启动"
    print_message "转发端口: 1234"
    print_message "指标端口: 9099"
    print_message "查看日志: docker logs -f tcp-forwarder"
}

# 使用docker-compose运行
run_compose() {
    print_step "使用Docker Compose启动..."
    
    COMPOSE_FILE=${1:-"docker-compose.simple.yml"}
    
    if [ ! -f "$COMPOSE_FILE" ]; then
        print_error "Docker Compose文件不存在: $COMPOSE_FILE"
        exit 1
    fi
    
    # 创建日志目录
    mkdir -p logs
    
    # 启动服务
    docker-compose -f "$COMPOSE_FILE" up -d
    
    print_message "服务已启动"
    print_message "查看状态: docker-compose -f $COMPOSE_FILE ps"
    print_message "查看日志: docker-compose -f $COMPOSE_FILE logs -f"
}

# 停止服务
stop_services() {
    print_step "停止服务..."
    
    # 停止docker-compose服务
    if [ -f "docker-compose.yml" ]; then
        docker-compose -f docker-compose.yml down 2>/dev/null || true
    fi
    
    if [ -f "docker-compose.simple.yml" ]; then
        docker-compose -f docker-compose.simple.yml down 2>/dev/null || true
    fi
    
    # 停止单独的容器
    docker stop tcp-forwarder 2>/dev/null || true
    docker rm tcp-forwarder 2>/dev/null || true
    
    print_message "服务已停止"
}

# 查看日志
show_logs() {
    print_step "查看日志..."
    
    if docker ps | grep -q tcp-forwarder; then
        docker logs -f tcp-forwarder
    else
        print_error "TCP转发器容器未运行"
        exit 1
    fi
}

# 显示帮助信息
show_help() {
    cat << EOF
Rust TCP智能转发器 - Docker部署脚本

用法: $0 [命令] [选项]

命令:
    build [tag]          构建Docker镜像（默认标签: tcp-forwarder:latest）
    run                  运行容器（简单模式）
    compose [file]       使用Docker Compose运行（默认: docker-compose.simple.yml）
    stop                 停止所有服务
    logs                 查看日志
    check                检查必要文件
    help                 显示此帮助信息

示例:
    $0 build                          # 构建镜像
    $0 build tcp-forwarder:v1.0      # 构建指定标签的镜像
    $0 run                            # 简单运行
    $0 compose                        # 使用简化compose运行
    $0 compose docker-compose.yml     # 使用完整compose运行
    $0 stop                           # 停止服务
    $0 logs                           # 查看日志

EOF
}

# 主函数
main() {
    case "${1:-help}" in
        "build")
            check_prerequisites
            build_image "$2"
            ;;
        "run")
            check_prerequisites
            build_image
            run_simple
            ;;
        "compose")
            check_prerequisites
            build_image
            run_compose "$2"
            ;;
        "stop")
            stop_services
            ;;
        "logs")
            show_logs
            ;;
        "check")
            check_prerequisites
            ;;
        "help"|"--help"|"-h")
            show_help
            ;;
        *)
            print_error "未知命令: $1"
            show_help
            exit 1
            ;;
    esac
}

# 执行主函数
main "$@"
