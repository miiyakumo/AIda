#!/usr/bin/env bash
#
# install-linux.sh — Alda Agent 环境安装脚本
#
# 检测 Linux 发行版，安装所需依赖（Java、Alda），并引导用户验证安装。
# 幂等：已安装的项自动跳过。
#
# 用法:
#   ./scripts/install-linux.sh
#   ./scripts/install-linux.sh --check
#

set -euo pipefail

if [[ "${1:-}" == "--help" ]]; then
    echo "用法: $0 [--check]"
    echo "  --check  只检查 Java、Alda 与 Rust，不安装或提权"
    exit 0
fi

if [[ "${1:-}" == "--check" ]]; then
    failures=0
    for program in java alda rustc; do
        if command -v "$program" >/dev/null 2>&1; then
            echo "[OK] $program: $(command -v "$program")"
        else
            echo "[ERR] 未找到 $program" >&2
            failures=$((failures + 1))
        fi
    done
    if command -v alda >/dev/null 2>&1; then
        alda version || failures=$((failures + 1))
    fi
    exit "$failures"
fi

if [[ $# -gt 0 ]]; then
    echo "未知参数: $1" >&2
    echo "用法: $0 [--check]" >&2
    exit 2
fi

# ──────────────────────────────────────────────
# 颜色输出
# ──────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

info()  { echo -e "${CYAN}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERR]${NC}   $*"; }

# ──────────────────────────────────────────────
# 检测 Linux 发行版
# ──────────────────────────────────────────────
detect_distro() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        DISTRO_ID="${ID}"
    else
        DISTRO_ID="unknown"
    fi

    case "$DISTRO_ID" in
        ubuntu|debian|linuxmint|pop|elementary|zorin)
            PKG_MGR="apt"
            INSTALL_CMD="sudo apt install -y"
            ;;
        fedora|rhel|centos|rocky|almalinux)
            PKG_MGR="dnf"
            INSTALL_CMD="sudo dnf install -y"
            ;;
        arch|manjaro|endeavouros)
            PKG_MGR="pacman"
            INSTALL_CMD="sudo pacman -S --noconfirm"
            ;;
        opensuse*|sles)
            PKG_MGR="zypper"
            INSTALL_CMD="sudo zypper install -y"
            ;;
        *)
            PKG_MGR="unknown"
            INSTALL_CMD=""
            ;;
    esac

    info "检测到发行版: ${DISTRO_ID}, 包管理器: ${PKG_MGR:-无}"
}

# ──────────────────────────────────────────────
# 确认 sudo 操作
# ──────────────────────────────────────────────
confirm_sudo() {
    local description="$1"
    echo ""
    warn "即将使用 sudo 执行: ${description}"
    warn "可能需要输入密码。"
    read -r -p "是否继续? [y/N] " REPLY
    if [[ ! "$REPLY" =~ ^[Yy]$ ]]; then
        info "已跳过。"
        return 1
    fi
    return 0
}

# ──────────────────────────────────────────────
# 安装 Java
# ──────────────────────────────────────────────
install_java() {
    if command -v java &>/dev/null; then
        local java_ver
        java_ver=$(java -version 2>&1 | head -1 || true)
        ok "Java 已安装: ${java_ver}"
        return 0
    fi

    warn "未检测到 Java。"

    if [ "$PKG_MGR" = "unknown" ]; then
        err "无法自动安装 Java。请手动安装 JDK 11+。"
        err "参考: https://adoptium.net/ 或 https://aws.amazon.com/corretto/"
        return 1
    fi

    local pkg=""
    case "$PKG_MGR" in
        apt)    pkg="default-jdk" ;;
        dnf)    pkg="java-11-openjdk" ;;
        pacman) pkg="jdk-openjdk" ;;
        zypper) pkg="java-11-openjdk" ;;
    esac

    local desc="安装 ${pkg} (JDK)"
    if ! confirm_sudo "$desc"; then
        err "Java 未安装，无法继续。请手动安装 JDK 11+ 后重新运行。"
        return 1
    fi

    info "正在安装 Java..."
    ${INSTALL_CMD} ${pkg}

    if command -v java &>/dev/null; then
        ok "Java 安装成功。"
    else
        err "Java 安装失败，请手动安装 JDK 11+。"
        return 1
    fi
}

# ──────────────────────────────────────────────
# 安装 Alda
# ──────────────────────────────────────────────
install_alda() {
    if command -v alda &>/dev/null; then
        local alda_ver
        alda_ver=$(alda version 2>&1 | head -1 || echo "unknown")
        ok "Alda 已安装: ${alda_ver}"
        return 0
    fi

    warn "未检测到 Alda。"
    err "脚本不会自动执行远程 curl | bash。"
    err "请从 https://alda.io/install 检查官方安装步骤，安装后重新运行本脚本。"
    return 1
}

# ──────────────────────────────────────────────
# 【可选】Rust 工具链
# ──────────────────────────────────────────────
# 从源码构建还需要 Rust 工具链。
# 如果尚未安装，运行:
#   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# 安装后重新加载环境:
#   source "$HOME/.cargo/env"

install_rust_toolchain() {
    if command -v rustc &>/dev/null; then
        local rust_ver
        rust_ver=$(rustc --version)
        ok "Rust 工具链已安装: ${rust_ver}"
        return 0
    fi

    info "Rust 工具链未安装（从源码构建需要）。如需安装，请运行:"
    info "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
}

# ──────────────────────────────────────────────
# 主流程
# ──────────────────────────────────────────────
main() {
    echo ""
    echo "=========================================="
    echo "  Alda Agent 环境安装脚本"
    echo "=========================================="
    echo ""

    detect_distro
    echo ""

    # 安装 Java（Alda 的运行时依赖）
    install_java || exit 1
    echo ""

    # 安装 Alda
    install_alda || exit 1
    echo ""

    # 【可选】Rust 工具链
    install_rust_toolchain

    # ──────────────────────────────────────────
    # 完成
    # ──────────────────────────────────────────
    echo ""
    echo "=========================================="
    echo "  安装完成！"
    echo "=========================================="
    echo ""
    info "请运行以下命令验证安装:"
    info "  cargo run -- doctor"
    echo ""
}

main "$@"
