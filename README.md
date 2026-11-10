# 剪贴板监控

一个在本机运行的跨平台剪贴板变化记录工具，支持 Windows、macOS 和 Linux。文本、图片等记录保存在本机 SQLite，界面默认只监听回环地址，不上传剪贴板内容。

## 平台能力

| 平台 | 变化检测 | 文本 | 图片 | 额外能力 |
| --- | --- | --- | --- | --- |
| Windows | `WM_CLIPBOARDUPDATE`，message-only 窗口 | Unicode 文本 | 系统图片转换为 PNG 元数据 | 完整内容摘要去重 |
| macOS | 每 200ms 检查 `NSPasteboard.changeCount` | Unicode 文本 | TIFF 等系统图片转换为 PNG 元数据 | 完整内容摘要去重 |
| Linux/X11 | XFixes 事件，空闲时不轮询内容 | 纯文本、富文本 | PNG/JPEG/GIF/WebP/BMP 等格式元数据 | PRIMARY、来源进程、ConvertSelection 读取审计 |

Windows 和 macOS 不提供可靠的复制者进程归属及全局读取审计，因此界面会隐藏这些 X11 专属功能。原生 Wayland 客户端不在当前 Linux/X11 后端能力范围内；XWayland 上只能看到走 X11 的程序。

图片只保存格式、尺寸、大小和摘要，不保存像素。Windows/macOS 上的 RGBA 图片会临时编码为 PNG 以取得尺寸和完整 SHA-256，随后丢弃编码字节。文本小于 1MB 时保存原文，达到 1MB 时只保存大小、摘要和前 4KB。文件列表和文件内容保存规则目前适用于 Linux/X11。

## 构建

安装当前稳定版 Rust 工具链后：

```bash
cargo build --release --locked
```

产物位于 `target/release/clipboard-monitor`；Windows 为 `target/release/clipboard-monitor.exe`。

SQLite 编进可执行文件，不需要单独安装数据库。Windows 发布构建静态链接 CRT；macOS 只使用系统框架；Linux 使用系统 glibc，不需要安装 X11 客户端开发库。

## 运行

前台运行：

```bash
./target/release/clipboard-monitor serve
```

不写子命令时等同于 `serve`。浏览器打开 <http://127.0.0.1:27890>，可以开关监控、按时间/类型筛选和清理日志。

```bash
./target/release/clipboard-monitor status
./target/release/clipboard-monitor stop
```

后台启动并记录 `monitor.log`：

```bash
./target/release/clipboard-monitor start
```

指定监听地址或数据目录：

```bash
./target/release/clipboard-monitor serve --listen 127.0.0.1:27890 --data-dir ./monitor-data
```

监听地址建议保持为 `127.0.0.1`。如果绑定到其它地址，日志可能被同一网络里的机器访问。

## 配置与数据目录

保留 `clipboard-x11` 目录名是为了兼容已有安装，并不表示新版本只支持 X11。

| 平台 | 配置目录 | 数据目录 |
| --- | --- | --- |
| Windows | `%APPDATA%\clipboard-x11` | `%LOCALAPPDATA%\clipboard-x11` |
| macOS | `~/Library/Application Support/clipboard-x11` | `~/Library/Application Support/clipboard-x11` |
| Linux | `$XDG_CONFIG_HOME/clipboard-x11`，或 `~/.config/clipboard-x11` | `$XDG_DATA_HOME/clipboard-x11`，或 `~/.local/share/clipboard-x11` |

数据库为数据目录下的 `monitor.db`；Unix 权限为 `0600`。使用 `--data-dir` 指定其它目录时，配置默认改到该目录下的 `config.json`，也可以用 `--config` 单独指定。

| 字段 | 默认 | 含义 |
| --- | --- | --- |
| `listen` | `127.0.0.1:27890` | 界面地址，修改后重启进程 |
| `retention_days` | `7` | 超过天数的记录自动删除 |
| `max_db_mb` | `500` | 库文件超过该大小时删除最旧记录 |
| `text_full_limit_bytes` | `1048576` | 文本完整保存上限 |
| `text_preview_bytes` | `4096` | 超限文本保留的前缀 |
| `file_content_limit_bytes` | `2097152` | X11 单文件内容保存上限 |
| `record_file_content` | `false` | 是否保存 X11 单个小文件内容 |
| `monitor_primary` | `true` | X11 是否监控 PRIMARY |
| `whitelist` | `[]` | X11 不记录的程序 |
| `enabled_on_start` | `true` | 启动时是否立即监控 |

自动清理约每 30 秒检查一次，当前仍被引用的最新内容不会被容量清理删成悬空记录。手动清理必须带时间、类型或操作中的至少一项：

```bash
./target/release/clipboard-monitor cleanup --from 2026-11-01 --to 2026-11-10T18:00:00 --type text
```

时间可以是 `YYYY-MM-DD`、`YYYY-MM-DDTHH:MM:SS`（本地时区），或 Unix 秒/毫秒。内容类型为 `text`、`richtext`、`image`、`files`、`custom`、`mixed`、`empty`。

## 日志与排障

默认输出启动、后端变化和错误信息，不打印剪贴板正文。开启调试日志：

```bash
CLIPBOARD_MONITOR_LOG=debug ./target/release/clipboard-monitor serve
```

Windows PowerShell：

```powershell
$env:CLIPBOARD_MONITOR_LOG = "debug"
.\clipboard-monitor.exe serve
```

- **Windows**：剪贴板被其它程序临时占用时会进行 3 次短重试；持续失败会写入 `last_error` 和日志。
- **macOS**：系统隐私设置可能要求允许终端或本程序访问剪贴板。授权后重启进程。
- **Linux**：需要 X11 会话以及 XFIXES/RECORD 扩展。可用 `echo "$XDG_SESSION_TYPE $DISPLAY"` 和 `xdpyinfo | grep -E 'RECORD|XFIXES'` 检查。RECORD 不可用只影响读取审计，XFixes 变化记录仍可运行。
- **界面状态**：`GET /api/status` 中的 `changes_ready` 表示变化监听是否就绪，`capabilities` 列出当前后端能力，`last_error` 给出最近一次异常。

## 测试

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
```

默认单元测试不读取或改写真实剪贴板。Linux/X11 实时集成测试会临时替换 CLIPBOARD，必须显式开启：

```bash
CLIPBOARD_MONITOR_LIVE=1 cargo test --locked --test flow -- --nocapture
```

GitHub Actions 会在每个原生系统上运行真实文本/图片监听测试、构建 release 二进制，并验证解压后的发布包可以直接执行。测试会临时替换 CLIPBOARD：

```bash
CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --release --test native_clipboard -- --ignored --nocapture
```

## 自动构建与 Releases

工作流位于 [.github/workflows/release.yml](.github/workflows/release.yml)：

- 推送任意分支、推送标签或手动触发时，构建 Windows x64、macOS ARM64/x64、Linux x64/ARM64。
- 所有构建先上传为 GitHub Actions artifacts。
- 推送标签时，自动创建对应 GitHub Release，上传五个平台的 `.zip`/`.tar.gz` 和 `SHA256SUMS`。
- 发布包包含对应架构的单文件可执行程序和本说明；Windows/macOS 包未做平台签名或公证。

例如发布 `v1.0.0`：

```bash
git tag v1.0.0
git push origin v1.0.0
```

## 已知限制

- Linux 仅支持 X11/XWayland，不支持原生 Wayland data-control。
- 程序如果在自己的进程里缓存旧内容，不再向 X 服务器请求，就不会产生 X11 读取记录。
- X11 所有者如果在内容被取走前退出，对应写入会留下“没有取到内容”的说明。
- macOS 使用轻量变化计数检查，检测延迟最多约 200ms，加内容抓取时间；Windows 消息循环每 50ms 检查一次系统通知。
- 同一次 X11 粘贴可能请求多个格式，300ms 内同一程序对同一选择区的读取会合并成一条。
