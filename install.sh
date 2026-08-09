#!/bin/bash
# Claude Code Sync - One-click installation script
# Usage: curl -fsSL https://raw.githubusercontent.com/osen77/cc-session/master/install.sh | bash

set -e

# Configuration
REPO="osen77/cc-session"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# Print functions
info() { echo -e "${CYAN}$1${NC}"; }
success() { echo -e "${GREEN}$1${NC}"; }
warn() { echo -e "${YELLOW}$1${NC}"; }
error() { echo -e "${RED}$1${NC}"; exit 1; }

echo ""
echo -e "${BOLD}${CYAN}🔧 ccs (Claude Code Sync) 安装程序${NC}"
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    darwin)
        OS_NAME="macOS"
        BINARY_OS="macos"
        ;;
    linux)
        OS_NAME="Linux"
        BINARY_OS="linux"
        ;;
    mingw*|msys*|cygwin*)
        error "Windows 请使用 PowerShell 安装:\n  irm https://raw.githubusercontent.com/${REPO}/master/install.ps1 | iex"
        ;;
    *)
        error "不支持的操作系统: $OS"
        ;;
esac

case "$ARCH" in
    x86_64|amd64)
        ARCH_NAME="x86_64"
        BINARY_ARCH="x86_64"
        ;;
    arm64|aarch64)
        ARCH_NAME="aarch64"
        BINARY_ARCH="aarch64"
        ;;
    *)
        error "不支持的架构: $ARCH"
        ;;
esac

info "检测到系统: ${OS_NAME} (${ARCH_NAME})"
echo ""

# Construct asset name (tar.gz format from release-new.yml)
ASSET_NAME="ccs-${BINARY_OS}-${BINARY_ARCH}.tar.gz"

# Get latest version
info "📦 获取最新版本..."

LATEST_VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')

if [ -z "$LATEST_VERSION" ]; then
    error "无法获取最新版本。请检查网络连接或稍后重试。"
fi

success "   最新版本: ${LATEST_VERSION}"
echo ""

# Check if already installed
if command -v ccs &> /dev/null; then
    CURRENT_VERSION=$(ccs --version 2>/dev/null | grep -oE 'v?[0-9]+\.[0-9]+\.[0-9]+' | head -1)
    if [ -n "$CURRENT_VERSION" ]; then
        info "   当前版本: ${CURRENT_VERSION}"

        # Simple version comparison
        CURRENT_CLEAN=$(echo "$CURRENT_VERSION" | sed 's/^v//')
        LATEST_CLEAN=$(echo "$LATEST_VERSION" | sed 's/^v//')

        if [ "$CURRENT_CLEAN" = "$LATEST_CLEAN" ]; then
            success "✓ 已是最新版本"
            echo ""
            read -p "是否重新安装? [y/N] " -n 1 -r
            echo ""
            if [[ ! $REPLY =~ ^[Yy]$ ]]; then
                info "已取消安装。"
                exit 0
            fi
        fi
        echo ""
    fi
fi

# Download
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_VERSION}/${ASSET_NAME}"

info "📥 正在下载..."
info "   ${DOWNLOAD_URL}"
echo ""

# Create install directory and temp directory
mkdir -p "$INSTALL_DIR"
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Download with progress
if curl -fSL --progress-bar "$DOWNLOAD_URL" -o "${TEMP_DIR}/${ASSET_NAME}"; then
    # Extract tar.gz
    tar -xzf "${TEMP_DIR}/${ASSET_NAME}" -C "${TEMP_DIR}"
    mv "${TEMP_DIR}/ccs" "${INSTALL_DIR}/ccs"
    chmod +x "${INSTALL_DIR}/ccs"
    success "✓ 下载完成"
else
    error "下载失败。请检查网络连接或稍后重试。"
fi

echo ""

# Add to PATH if needed
if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
    warn "⚠️  ${INSTALL_DIR} 不在 PATH 中"
    echo ""

    # Detect shell and update config
    SHELL_NAME=$(basename "$SHELL")
    case "$SHELL_NAME" in
        zsh)
            SHELL_RC="$HOME/.zshrc"
            ;;
        bash)
            if [ -f "$HOME/.bashrc" ]; then
                SHELL_RC="$HOME/.bashrc"
            else
                SHELL_RC="$HOME/.bash_profile"
            fi
            ;;
        *)
            SHELL_RC="$HOME/.profile"
            ;;
    esac

    read -p "是否自动添加到 PATH? [Y/n] " -n 1 -r
    echo ""

    if [[ ! $REPLY =~ ^[Nn]$ ]]; then
        echo "" >> "$SHELL_RC"
        echo "# Claude Code Sync" >> "$SHELL_RC"
        echo "export PATH=\"\$PATH:${INSTALL_DIR}\"" >> "$SHELL_RC"
        success "✓ 已添加到 ${SHELL_RC}"
        info "   请运行: source ${SHELL_RC}"
        info "   或重新打开终端"
        echo ""

        # Export for current session
        export PATH="$PATH:${INSTALL_DIR}"
    else
        info "请手动添加到 PATH:"
        echo "   export PATH=\"\$PATH:${INSTALL_DIR}\""
        echo ""
    fi
fi

# Verify installation
echo ""
info "验证安装..."

if "${INSTALL_DIR}/ccs" --version &> /dev/null; then
    VERSION=$("${INSTALL_DIR}/ccs" --version 2>/dev/null)
    success "✓ ${VERSION}"
else
    error "安装验证失败"
fi

echo ""
echo -e "${CYAN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
success "🎉 安装完成！"
echo ""

# Check if already configured
if "${INSTALL_DIR}/ccs" status &> /dev/null 2>&1; then
    success "✓ 已检测到现有配置"
    echo ""
    info "常用命令:"
    echo "   ccs sync   - 双向同步"
    echo "   ccs status - 查看状态"
    echo "   ccs update - 检查更新"
else
    echo ""
    read -p "是否立即配置? [Y/n] " -n 1 -r
    echo ""

    if [[ ! $REPLY =~ ^[Nn]$ ]]; then
        echo ""
        "${INSTALL_DIR}/ccs" setup
    else
        echo ""
        info "稍后运行 'ccs setup' 进行配置"
    fi
fi

echo ""
