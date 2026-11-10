# 剪贴板监控

一个在本机运行的跨平台剪贴板变化记录工具，支持 Windows、macOS 和 Linux。文本、图片等记录保存在本机 SQLite，界面默认只监听回环地址，不上传剪贴板内容。

## 平台能力

| 平台 | 变化检测 | 文本 | 图片 | 事件与元数据 |
| --- | --- | --- | --- | --- |
| Windows | `WM_CLIPBOARDUPDATE` + `GetClipboardSequenceNumber` 廉价预检，内容摘要兜底 | Unicode 文本 | 系统图片记录尺寸与完整摘要 | 内容/清空/格式/序号变化区分；格式清单；剪贴板所有者 → PID / 进程路径；前台触发主体；`GetOpenClipboardWindow` 轮询 + Ctrl+V 钩子交叉推断的**推断读取** |
| macOS | `NSPasteboard.changeCount` 快速路径 + 最多每 2 秒内容摘要采样兜底（无窗口会话兼容） | Unicode 文本 | TIFF 等系统图片记录尺寸与完整摘要 | 内容/清空/格式/序号变化区分；格式清单；前台应用 PID / 名称 / 路径 |
| Linux/X11 | XFixes 事件，空闲时不轮询内容 | 纯文本、富文本 | PNG/JPEG/GIF/WebP/BMP 等格式元数据 | PRIMARY（默认关闭，见 `monitor_primary`）、XRes 来源进程、ConvertSelection **可靠**读取审计、读写流转链 |
| Linux/Wayland | `ext-data-control-v1` 的 selection 事件（需 compositor 授权绑定） | UTF-8 文本 | 图片格式元数据 | 格式清单；**无**读取审计、**无**来源进程、**无** PRIMARY（默认） |

**事件语义（三平台一致）**：`op` 只有 `write` / `read` 两种。写入事件的 `reason` 区分 `snapshot`（启动快照）、`content_change`、`cleared`、`format_change`（内容相同但格式集合变了）、`sequence_change`（内容与格式相同但 OS 变更序号变了）、`reenabled`（暂停后恢复）；X11 另有 `owner_change` / `window_destroy` / `client_close`。快速连续复制 A→B→A 不会被最终状态吞掉。所有事件在 `meta_json.change` 里带判定依据，`meta_json.formats` / `sequence` 记录格式清单与 OS 序号。

**读取能力的诚实边界**：只有 Linux/X11 能通过 RECORD 拦截 `ConvertSelection`，得到可靠的全局读取审计（`capabilities.reads=true`）。

Windows 的 `capabilities.reads_inferred=true` 是**尽力而为的推断**，因为 Win32 在读取粘贴时不发任何系统通知（`WM_CLIPBOARDUPDATE` 只在内容变化时触发，序号也不随读取递增）。`read_signal.rs` 用两条间接信号交叉验证：

1. 每 100ms 轮询 `GetOpenClipboardWindow()`：任何读取/粘贴都必须先 `OpenClipboard`；当别的进程持有剪贴板、且剪贴板变更序号没变（说明不是写入）时，视为疑似读取，并取该窗口的 PID / 进程路径作为读取方。
2. `WH_KEYBOARD_LL` 低级键盘钩子捕获 Ctrl+V：作为独立佐证，补足轮询可能漏掉的极短读取；钩子回调只写原子时间戳，不阻塞。

两信号在 300ms 窗口内去重，落库为 `op=read` + `reason=inferred_read`，摘要里写明判定依据（`open_clipboard` / `ctrl_v` / `both`），界面显示"推断读取"。**它不是可靠的全局审计**：菜单粘贴、自绘控件的直接 API 读取可能漏掉；剪贴板管理器/RDP 同步也可能被算作读取；钩子在权限受限或会话隔离时可能装不上，此时仅靠轮询降级运行。macOS 不产生任何读取事件。界面据此隐藏/放开"读取"筛选与流转链。

**元数据（OS 可可靠提供的全部）**：Windows 采集 `GetClipboardOwner` → `GetWindowThreadProcessId` → `QueryFullProcessImageNameW`（写入者 PID / 应用名 / 可执行路径 / 窗口句柄），以及 `GetForegroundWindow` + `GetWindowTextW`（触发主体）；macOS 采集 `NSWorkspace.frontmostApplication` + `proc_pidpath`（前台应用 PID / 名称 / bundle id / 路径）。无窗口/headless 会话里若剪贴板没有所有者窗口，Windows 会回退到前台应用并在 `meta_json.source.attribution` 标为 `foreground_fallback`（`window_id` 留 0，不冒充所有者）；两者都取不到时标 `unavailable` 并写明原因，保证每条记录都能解释自己的元数据完整度，绝不填误导性数值。`wm_class` 是 X11 概念，Windows/macOS 恒为空；`window_id` 在 macOS 没有对应概念，恒为 0。来源与触发主体另外写进 `meta_json.source` / `meta_json.trigger`，详情页展示归属方式。

**白名单三平台生效**：命中白名单的程序，其剪贴板正文零落盘（不产生 contents / events 行），与 Linux/X11 行为一致。`wm_class` 匹配只在 X11 有意义，其它平台按进程名、可执行文件名/全路径、PID 匹配。

**Linux 后端自动选择**：启动时若检测到 `WAYLAND_DISPLAY` 且 compositor 提供 `ext_data_control_manager_v1` 全局，使用 Wayland 后端；否则回退 X11/XWayland（`/api/status` 的 `backend` 显示实际生效者）。Wayland 是"特权客户端"协议，KDE/sway 等通常可用，GNOME 可能只允许内置剪贴板管理器而拒绝绑定 —— 绑定失败会自动回退 X11，不会导致启动失败。`capabilities.reads` 与 `capabilities.primary` 在 Wayland 后端下为 false，**Wayland 下没有读取审计与来源进程归属**，这是协议限制而非实现缺失。原生 Wayland 客户端由此后端覆盖，XWayland 上的程序仍走 X11 后端。

图片只保存格式、尺寸、大小和摘要，不保存像素。Windows/macOS 直接对剪贴板解码出的 RGBA 像素计算完整 SHA-256（宽高参与摘要），不做二次编码。文本小于 1MB 时保存原文，达到 1MB 时只保存大小、摘要和前 4KB。文件列表和文件内容保存规则目前适用于 Linux/X11。

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

### 开机自启与服务化

`autostart` 用三平台原生方式配置**当前用户**的开机自启，不需要管理员权限，也不写系统目录：

```bash
./target/release/clipboard-monitor autostart enable    # 启用
./target/release/clipboard-monitor autostart status    # 查看状态与配置位置
./target/release/clipboard-monitor autostart disable   # 禁用（只删除本工具写入的那一项）
```

| 平台 | 配置位置 |
| --- | --- |
| Linux | `~/.config/autostart/clipboard-monitor.desktop`（XDG Autostart） |
| macOS | `~/Library/LaunchAgents/com.clipboard-monitor.plist`（launchd） |
| Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\ClipboardMonitor` |

`service` 只把可直接安装的单元内容打印到 stdout，**不写系统目录**，由用户自行决定如何安装：

```bash
# Linux (systemd --user)
./target/release/clipboard-monitor service unit > ~/.config/systemd/user/clipboard-monitor.service
systemctl --user daemon-reload && systemctl --user enable --now clipboard-monitor

# macOS (launchd)
./target/release/clipboard-monitor service plist > ~/Library/LaunchAgents/com.clipboard-monitor.service.plist
launchctl load ~/Library/LaunchAgents/com.clipboard-monitor.service.plist
```

Windows 用 `autostart enable`（HKCU Run）即可；`service` 不提供 Windows 服务输出。启用 `autostart` 时若命令行带过 `--data-dir` / `--config`，会把这些参数写进自启命令，保证数据目录一致。

监听地址建议保持为 `127.0.0.1`。如果绑定到其它地址，日志可能被同一网络里的机器访问。

## 配置与数据目录

保留 `clipboard-x11` 目录名是为了兼容已有安装，并不表示新版本只支持 X11。

| 平台 | 配置目录 | 数据目录 |
| --- | --- | --- |
| Windows | `%APPDATA%\clipboard-x11` | `%LOCALAPPDATA%\clipboard-x11` |
| macOS | `~/Library/Application Support/clipboard-x11` | `~/Library/Application Support/clipboard-x11` |
| Linux | `$XDG_CONFIG_HOME/clipboard-x11`，或 `~/.config/clipboard-x11` | `$XDG_DATA_HOME/clipboard-x11`，或 `~/.local/share/clipboard-x11` |

数据库为数据目录下的 `monitor.db`；Unix 权限为 `0600`。使用 `--data-dir` 指定其它目录时，配置默认改到该目录下的 `config.json`，也可以用 `--config` 单独指定。

### 正文加密与全文检索

剪贴板正文（文本、图片 blob、`meta_json`、事件摘要）以 **AES-256-GCM** 加密后落库，SQLite 文件里 grep 不到明文密码；时间、类型、应用名、PID 等元数据列保持明文以便筛选与索引。密钥是 32 字节随机值：

| 平台 | 密钥保管 | 回落 |
| --- | --- | --- |
| Windows | DPAPI（`master.key.dpapi`，绑定当前用户账户） | 数据目录 `master.key` |
| macOS | Keychain 通用密码项（按数据目录区分账户） | 数据目录 `master.key` |
| Linux | 数据目录 `master.key`（`0600`） | — |

密钥文件/DPAPI blob 与数据库同目录，**备份或迁移数据目录时必须一起复制**，否则无法解密。旧版本明文库在首次用新版本打开时会自动重加密全部历史行，API 读取兼容未加密的历史行（渐进迁移）；密钥丢失或不匹配会明确报错而不是静默丢数据。这是**字段级加密**，不是全库加密：能同时拿到数据库与密钥的攻击者仍可解密；需要全库加密请用 SQLCipher 另行编译。

搜索框由 SQLite **FTS5**（`trigram` 分词，索引解密后的明文）驱动，支持中文子串与英文单词；≥3 字符的查询走全文索引，短查询退回应用名/格式的 `LIKE` 匹配。索引随写入增量维护，迁移后自动重建。

| 字段 | 默认 | 含义 |
| --- | --- | --- |
| `listen` | `127.0.0.1:27890` | 界面地址，修改后重启进程 |
| `retention_days` | `7` | 超过天数的记录自动删除 |
| `max_db_mb` | `500` | 库文件超过该大小时删除最旧记录 |
| `text_full_limit_bytes` | `1048576` | 文本完整保存上限 |
| `text_preview_bytes` | `4096` | 超限文本保留的前缀 |
| `file_content_limit_bytes` | `2097152` | X11 单文件内容保存上限 |
| `record_file_content` | `false` | 是否保存 X11 单个小文件内容 |
| `monitor_primary` | `false` | X11 是否监控 PRIMARY（选中文字即写入、中键粘贴即读取，开启后选中操作会产生多条记录，普通使用建议关闭） |
| `whitelist` | `[]` | 不记录的程序（三平台生效，命中即正文零落盘） |
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

- **Windows**：剪贴板被其它程序临时占用时会短重试；单次抓取有 5 秒超时保护，不会卡死监听线程。监听窗口初始化失败会带退避重试（不再需要重启进程）。持续失败会写入 `last_error` 并把 `changes_ready` 置为 false。`GetClipboardOwner` 在延迟渲染下返回的是"声明所有权"的窗口，不一定是发起复制的线程；`reason=inferred_read` 的读取事件只是间接推断，不能当作可靠审计。
- **macOS**：系统隐私设置可能要求允许终端或本程序访问剪贴板。授权后重启进程。正常窗口会话用 `NSPasteboard.changeCount` 即时发现变化；无窗口或 SSH 会话里该计数可能不递增，因此程序仍会最多每 2 秒读取一次文本/图片并计算摘要作为兜底，只有摘要或格式变化时才写日志。`changes_ready` 只在首次成功抓取后为 true，隐私权限失败不会再显示"监控中"。
- **Linux**：需要 X11 会话以及 XFIXES/RECORD 扩展。可用 `echo "$XDG_SESSION_TYPE $DISPLAY"` 和 `xdpyinfo | grep -E 'RECORD|XFIXES'` 检查。RECORD 不可用只影响读取审计，XFixes 变化记录仍可运行。
- **界面状态**：`GET /api/status` 中的 `changes_ready` 表示变化监听是否就绪，`capabilities` 列出当前后端能力（`reads` 可靠读取、`reads_inferred` 推断读取、`source_info` 来源信息、`formats` 格式清单、`primary` PRIMARY），`last_error` 给出最近一次异常。

## 常用任务

安装 [just](https://just.systems/) 后运行 `just --list` 查看全部快捷任务：

```bash
just check                 # 格式、Clippy、单元测试
just build                 # 构建 release 二进制
just smoke                 # 构建并执行 --help
just run "--listen 127.0.0.1:27891"
just test-native           # 真实系统剪贴板文本/图片测试，会替换剪贴板
just test-x11              # Linux/X11 完整读写流转测试
just verify-release        # 下载并校验已发布的包及 SHA256SUMS
```

`start`、`status`、`stop` 也分别对应同名命令。`just` 在 Windows 使用 PowerShell，在其它平台使用 Bash；发布校验任务需要已登录的 GitHub CLI (`gh`)。

## 测试

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
```

默认单元测试不读取或改写真实剪贴板。Linux/X11 实时集成测试会临时替换 CLIPBOARD，必须显式开启：

```bash
CLIPBOARD_MONITOR_LIVE=1 cargo test --locked --test flow -- --nocapture
```

GitHub Actions 会在每个原生系统上运行真实剪贴板契约测试（文本/图片写入、去重、清空语义、来源元数据、能力声明；结束后恢复原剪贴板）、格式与 Clippy 门禁、构建 release 二进制，并验证解压后的发布包可以直接执行。测试会临时替换 CLIPBOARD：

```bash
CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --release --test native_clipboard -- --ignored --nocapture
```

## 跨平台一致性校验标准

CI 在五个原生架构上按同一份契约校验，平台专有能力单独声明、不互相强求：

| # | 校验项 | 三平台共同要求 | 平台差异 |
| --- | --- | --- | --- |
| 1 | 事件类型完整性 | 每次内容变化恰好一条 `write`；`op` 只允许 `write`/`read`；清空、格式变化、序号变化、启动快照、暂停恢复各有明确 `reason` | 只有 X11 有可靠 `read`（RECORD）；Windows 的 `read` 由 `GetOpenClipboardWindow` 轮询 + Ctrl+V 钩子推断，恒为 `reason=inferred_read` 并在摘要写明依据；macOS 无 `read` |
| 2 | 元数据完整性 | `NewEvent` 列集合相同；来源/触发主体写入 `meta_json.source`/`trigger`；无法可靠采集的字段显式留空，不填误导值 | `wm_class` 仅 X11；macOS `window_id` 恒 0；Windows 用剪贴板所有者、macOS 用前台应用 |
| 3 | 去重与内容保真 | 同一内容+格式+序号连续通知不产生重复行；`body`/`blob`/`meta_json`/`sha256`/截断规则由共享 `classify::assemble` 保证一致 | X11 按所有权变化记录，不要求内容级去重；Win/mac 按指纹（内容+格式+序号）去重 |
| 4 | 异常恢复与可观测性 | 后端失效要么自动恢复并把 `changes_ready` 置真，要么写入 `last_error` 并置假；不得静默死亡；单次抓取有超时保护 | X11 有 `x_supervisor` 重连；Win/mac 有监听初始化退避重试、连续失败降级 |
| 5 | 白名单（隐私） | 命中白名单的程序正文零落盘（不插 contents/events），三平台一致 | 匹配维度：X11 多一个 `WM_CLASS`，其余按进程名/路径/PID |
| 6 | API 契约 | `/api/events`、`/api/events/{id}`、`/api/status` 的字段集合与类型一致；`capabilities` 必须与实际能力相符，不得谎报 | `reads`/`primary` 仅 X11 为真；`source_info`/`formats` 在 Win/mac 为真；`reads_inferred` 仅 Windows 为真 |
| 7 | 清理与保留期 | `keep_content_ids()` 正确保护当前快照；手动清理拒绝无条件删除；事务化落库不产生孤儿 contents | 无平台差异 |

**验收方式**：`cargo fmt --all -- --check`、`cargo clippy --locked --all-targets -- -D warnings`（CI 五个 target 各跑一次）、`cargo test --locked --all-targets`（含平台无关的观察契约单测），以及 `CLIPBOARD_MONITOR_NATIVE_LIVE=1 cargo test --release --test native_clipboard -- --ignored`（五架构真实剪贴板，覆盖文本/图片写入、去重、清空语义、元数据、能力声明，并在结束时恢复原剪贴板）。Linux/X11 另有 `CLIPBOARD_MONITOR_LIVE=1 cargo test --test flow` 验证读写流转链。

## 自动构建与 Releases

[v1.0.0-rc.3](https://github.com/zhengmingliang/clipboard-monitor/releases/tag/v1.0.0-rc.3) 已通过 [Actions 42826384539](https://github.com/zhengmingliang/clipboard-monitor/actions/runs/42826384539) 完成五架构原生构建、真实文本/图片监听、解压包运行和自动发布。发布包共五个，另有 `SHA256SUMS`；下载后的校验和已全部复核，Linux x64 发布包已再次解压直接运行验证。

工作流位于 [.github/workflows/release.yml](.github/workflows/release.yml)：

- 推送任意分支、推送标签或手动触发时，构建 Windows x64、macOS ARM64/x64、Linux x64/ARM64。
- 所有构建先上传为 GitHub Actions artifacts。
- 推送标签时，自动创建对应 GitHub Release，上传五个平台的 `.zip`/`.tar.gz` 和 `SHA256SUMS`。
- 每个构建都会执行原生文本/图片监听测试，并从解压后的发布包运行可执行文件；Linux 使用 Xvfb 提供 X11 会话。
- 发布包包含对应架构的单文件可执行程序和本说明；Windows/macOS 包未做平台签名或公证。

例如发布 `v1.0.0`：

```bash
git tag v1.0.0
git push origin v1.0.0
```

## 已知限制

- Linux 原生 Wayland 后端依赖 compositor 支持并授权 `ext-data-control-v1`；不支持的 compositor 会自动回退 X11/XWayland。Wayland 后端无读取审计、无来源进程归属、无 PRIMARY 与文件列表。
- X11 的 PRIMARY 选择区在选中文字时就会写入、中键粘贴时产生读取，开启 `monitor_primary` 后会出现大量由单纯选中操作引发的写入/读取记录；默认已关闭，旧配置里若为 `true` 请在设置中取消勾选。
- 程序如果在自己的进程里缓存旧内容，不再向 X 服务器请求，就不会产生 X11 读取记录。
- X11 所有者如果在内容被取走前退出，对应写入会留下“没有取到内容”的说明。
- Windows/macOS 的 OS 不提供全局读取拦截：Windows 只有 `reason=inferred_read` 的间接推断，macOS 完全没有读取事件；两者都不等价于 X11 的可靠读取审计。
- Windows/macOS 的内容变化通知不携带内容本身，极短时间内连续多次复制仍可能只采到最终状态；`GetClipboardSequenceNumber` / `changeCount` 快速路径已尽量缩短采样窗口，兜底采样间隔最多约 2 秒。
- Windows/macOS 不采集 `wm_class`（X11 概念）；macOS 没有窗口句柄概念，`window_id` 恒为 0。延迟渲染下 Windows 的剪贴板所有者不一定是发起复制的线程。
- Windows/macOS/Wayland 不保存文件列表/文件内容与富文本；PRIMARY 与文件列表仍是 Linux/X11 专属。
- Windows 的 `stop` 命令用 `TerminateProcess` 结束后台进程，不会执行 Unix 那样的优雅退出与 SQLite WAL checkpoint；WAL 本身持久有效，下次启动会正常恢复，记录不会丢失。
- 同一次 X11 粘贴可能请求多个格式，300ms 内同一程序对同一选择区的读取会合并成一条。
