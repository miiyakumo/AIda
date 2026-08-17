#!/usr/bin/env bash
#
# install-macos.sh — Alda Agent macOS 环境安装脚本
#
# 用法:
#   ./scripts/install-macos.sh
#   ./scripts/install-macos.sh --check

set -euo pipefail

SOUNDFONT_URL="https://raw.githubusercontent.com/mrbumpy409/GeneralUser-GS/684543d5e5efaef08d02be50dcda8d552478fa60/GeneralUser-GS.sf2"
SOUNDFONT_SHA256="9575028c7a1f589f5770fccc8cff2734566af40cd26ed836944e9a5152688cfe"

usage() {
    echo "用法: $0 [--check]"
    echo "  --check  只检查 Java、Alda、FluidSynth、GM SoundFont 与 Rust，不安装"
}

if [[ "${1:-}" == "--help" ]]; then
    usage
    exit 0
fi
if [[ $# -gt 0 && "${1:-}" != "--check" ]]; then
    echo "未知参数: $1" >&2
    usage >&2
    exit 2
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "此脚本仅支持 macOS；Linux 请运行 scripts/install-linux.sh" >&2
    exit 1
fi

default_soundfont_path() {
    if [[ -n "${XDG_DATA_HOME:-}" ]]; then
        printf '%s\n' "${XDG_DATA_HOME}/alda-agent/soundfonts/GeneralUser-GS.sf2"
    else
        printf '%s\n' "${HOME}/Library/Application Support/alda-agent/soundfonts/GeneralUser-GS.sf2"
    fi
}

find_tool() {
    local name="$1"
    local candidate
    if command -v "$name" >/dev/null 2>&1; then
        command -v "$name"
        return 0
    fi
    for candidate in "/opt/homebrew/bin/${name}" "/usr/local/bin/${name}"; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

find_java() {
    local candidate
    for candidate in /opt/homebrew/opt/openjdk/bin/java /usr/local/opt/openjdk/bin/java; do
        if [[ -x "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    candidate="$(command -v java 2>/dev/null || true)"
    if [[ -n "$candidate" && "$candidate" != "/usr/bin/java" ]]; then
        printf '%s\n' "$candidate"
        return 0
    fi
    if [[ -n "$candidate" && -x /usr/libexec/java_home ]] && /usr/libexec/java_home >/dev/null 2>&1; then
        printf '%s\n' "$candidate"
        return 0
    fi
    return 1
}

find_soundfont() {
    local candidate
    for candidate in "${ALDA_AGENT_SOUNDFONT:-}" "$(default_soundfont_path)"; do
        if [[ -n "$candidate" && -f "$candidate" ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

check_environment() {
    local failures=0
    local path
    if path="$(find_java)"; then
        echo "[OK] java: $path"
    else
        echo "[ERR] 未找到可用的 Java" >&2
        failures=$((failures + 1))
    fi
    for program in alda fluidsynth rustc; do
        if path="$(find_tool "$program")"; then
            echo "[OK] $program: $path"
        else
            echo "[ERR] 未找到 $program" >&2
            failures=$((failures + 1))
        fi
    done
    if path="$(find_soundfont)"; then
        echo "[OK] GM SoundFont: $path"
    else
        echo "[ERR] 未找到 GM SoundFont；可设置 ALDA_AGENT_SOUNDFONT" >&2
        failures=$((failures + 1))
    fi
    return "$failures"
}

if [[ "${1:-}" == "--check" ]]; then
    check_environment
    exit $?
fi

if ! command -v brew >/dev/null 2>&1; then
    echo "[ERR] 未找到 Homebrew。请先从 https://brew.sh 安装，再重新运行本脚本。" >&2
    exit 1
fi

packages=()
alda_missing=0
if ! find_tool alda >/dev/null; then
    packages+=(alda)
    alda_missing=1
fi
if ! find_tool fluidsynth >/dev/null; then
    packages+=(fluid-synth)
fi
if ! find_java >/dev/null && [[ "$alda_missing" -eq 0 ]]; then
    packages+=(openjdk)
fi
if [[ ${#packages[@]} -gt 0 ]]; then
    echo "[INFO] 正在通过 Homebrew 安装: ${packages[*]}"
    brew install "${packages[@]}"
fi

if ! find_java >/dev/null || ! find_tool alda >/dev/null || ! find_tool fluidsynth >/dev/null; then
    echo "[ERR] Homebrew 安装完成后仍未找到 Java、Alda 或 FluidSynth。" >&2
    exit 1
fi

soundfont_path="$(find_soundfont || true)"
if [[ -z "$soundfont_path" ]]; then
    soundfont_path="$(default_soundfont_path)"
    soundfont_dir="$(dirname "$soundfont_path")"
    mkdir -p "$soundfont_dir"
    temp_file="$(mktemp "${soundfont_dir}/GeneralUser-GS.sf2.XXXXXX")"
    trap 'rm -f "$temp_file"' EXIT
    echo "[INFO] 正在下载 GeneralUser GS SoundFont"
    curl --fail --location --output "$temp_file" "$SOUNDFONT_URL"
    actual_sha="$(shasum -a 256 "$temp_file" | awk '{print $1}')"
    if [[ "$actual_sha" != "$SOUNDFONT_SHA256" ]]; then
        echo "[ERR] SoundFont SHA-256 校验失败" >&2
        exit 1
    fi
    mv "$temp_file" "$soundfont_path"
    trap - EXIT
fi

echo "[OK] Alda Agent 运行环境已就绪"
echo "[INFO] SoundFont: $soundfont_path"
echo "[INFO] 请运行 cargo run -- doctor 验证安装"
