# 剪贴板监控常用任务。运行 `just --list` 查看全部命令。

set unstable := true

# Windows 使用 PowerShell；其它平台使用 Bash 严格模式。
set windows-powershell := true
set shell := ["bash", "-euo", "pipefail", "-c"]

exe_suffix := if os() == "windows" { ".exe" } else { "" }
exe := "target/release/clipboard-monitor" + exe_suffix

# 默认展示帮助。
default:
    @just --list

# 格式化代码。
fmt:
    cargo fmt --all

# 检查格式、静态分析和全部单元测试。
check:
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked --all-targets

# 构建 release 二进制。
build:
    cargo build --locked --release

# 前台运行，参数会原样传给 serve（例如 just run "--listen 127.0.0.1:27891"）。
run *args:
    {{exe}} serve {{args}}

# 以调试日志前台运行。
debug *args:
    CLIPBOARD_MONITOR_LOG=debug {{exe}} serve {{args}}

# 后台启动、查看状态、停止。
start *args:
    {{exe}} start {{args}}

status *args:
    {{exe}} status {{args}}

stop *args:
    {{exe}} stop {{args}}

# 真实系统剪贴板文本/图片端到端测试；会临时替换当前剪贴板。
test-native:
    CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --locked --release --test native_clipboard -- --ignored --nocapture --test-threads=1

# Linux/X11 完整读写流转测试；会临时替换当前 CLIPBOARD。
test-x11:
    CLIPBOARD_MONITOR_LIVE=1 cargo test --locked --test flow -- --nocapture

# 在 Xvfb 中运行原生端到端测试，适合无桌面 Linux 环境。
test-native-xvfb:
    xvfb-run -a -s "-screen 0 1280x1024x24 +extension XFIXES +extension RECORD" just test-native

# 构建并运行二进制的 --help，验证可直接启动。
smoke: build
    {{exe}} --help

# 从 GitHub Release 下载并校验压缩包；tag 默认为 v1.0.0-rc.3。
verify-release tag="v1.0.0-rc.3" repo="zhengmingliang/clipboard-monitor":
    mkdir -p target/release-assets
    gh release download {{tag}} --repo {{repo}} --dir target/release-assets --clobber
    cd target/release-assets && sha256sum -c SHA256SUMS
