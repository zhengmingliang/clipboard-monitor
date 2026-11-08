# 剪贴板监控

在本机 X11 会话里记录其它程序对剪贴板的写入和读取。日志存在本机 SQLite，界面只提供给本机浏览器。程序不上传剪贴板内容。

## 能记录什么

- 写入：某个程序成为 `CLIPBOARD` 或 `PRIMARY` 的所有者，或选择区被清空。
- 读取：某个程序向当前所有者请求剪贴板内容（`ConvertSelection`）。
- 流转：每次读取记下内容来自哪一次写入、来源程序和来源进程。
- 类型：纯文本、富文本、图片、文件列表、自定义格式、混合内容、空。

文本小于 1MB 时保存原文，达到 1MB 时只保存大小、摘要和前 4KB。文件默认只记数量、名称和路径；单个小于 2MB 的普通文件可以在设置里打开“保存文件内容”。图片和其它二进制只记格式、尺寸和大小，不保存原始字节。

桌面自带的剪贴板管理器经常会立刻把所有权接过去。这时内容仍向当时的所有者抓取，原写入记录保留最初的程序。

## 环境

需要 X11，并且 X 服务器打开了 XFIXES 和 RECORD。本机用下面的命令确认：

```bash
echo "$XDG_SESSION_TYPE $DISPLAY"
xdpyinfo | grep -E 'RECORD|XFIXES'
```

Wayland 会话没有 `DISPLAY` 时程序会直接退出。XWayland 上只能看到走 X11 的程序，看不到原生 Wayland 客户端。

监控程序自己的读取不会记入日志。白名单里的程序同样跳过。

## 编译

只依赖系统里的 Rust 工具链和 libc，SQLite 编进可执行文件，不需要单独安装数据库。

```bash
cargo build --release
```

生成的程序是 `target/release/clipboard-monitor`。

## 运行

```bash
./target/release/clipboard-monitor start
./target/release/clipboard-monitor status
```

浏览器打开状态里打印的地址，默认是 <http://127.0.0.1:27890>。页面上可以开关监控、按程序 / 时间 / 类型筛选、编辑白名单、按条件清理。

```bash
./target/release/clipboard-monitor stop
```

`serve` 在前台运行，适合看日志。不写子命令时等同于 `serve`。

```bash
./target/release/clipboard-monitor serve --listen 127.0.0.1:27890
```

监听地址建议保持在 `127.0.0.1`。如果绑到其它地址，日志可能被同一网络里的机器访问。

## 配置

默认配置文件：`~/.config/clipboard-x11/config.json`  
默认数据目录：`~/.local/share/clipboard-x11`  
数据库：`monitor.db`，权限 `0600`。

目录名不用 `clipboard-monitor`。本机另一套剪贴板工具已经占用了 `~/.config/clipboard-monitor` 和 `~/.local/share/clipboard-monitor`。

用 `--data-dir` 指定其它数据目录时，配置文件默认改到该目录下的 `config.json`，避免和正在使用的配置混在一起。

| 字段 | 默认 | 含义 |
| --- | --- | --- |
| `listen` | `127.0.0.1:27890` | 界面地址。修改后要重启进程 |
| `retention_days` | `7` | 超过天数的记录自动删除 |
| `max_db_mb` | `500` | 库文件超过该大小时删除最旧记录 |
| `text_full_limit_bytes` | `1048576` | 文本完整保存的上限 |
| `text_preview_bytes` | `4096` | 超限文本保留的前缀 |
| `file_content_limit_bytes` | `2097152` | 允许内联保存的单文件上限 |
| `record_file_content` | `false` | 是否保存小于上限的单个普通文件 |
| `monitor_primary` | `true` | 是否同时监控鼠标中键选中的 PRIMARY |
| `whitelist` | `[]` | 不记录的程序。可写进程名、WM_CLASS、可执行文件名，或进程号 |
| `enabled_on_start` | `true` | 进程启动时是否立即监控 |

白名单每行一条，名字不区分大小写。例如 `keepassxc`。多个程序同名时（几个 Python 脚本都会显示成 `python3`），写 `pid:4242` 或单独一行 `4242`，只排除这个进程号。进程重启后 PID 会变。监控进程自身始终排除。

自动清理大约每 30 秒检查一次。当前仍被引用的最新内容不会被容量清理删成悬空记录。

手动清理必须带时间、类型或操作中的至少一项，避免一次删光。命令行示例：

```bash
./target/release/clipboard-monitor cleanup --from 2026-11-01 --to 2026-11-04T18:00:00 --type text
```

时间可以是 `YYYY-MM-DD`、`YYYY-MM-DDTHH:MM:SS`（本地时区），或 Unix 秒 / 毫秒。

内容类型：`text`、`richtext`、`image`、`files`、`custom`、`mixed`、`empty`。

## 占用和延迟

空闲时等待 X 事件，不轮询剪贴板。事件先进入约 40ms 的排序缓冲，再写入数据库，避免同一次粘贴的读取比写入更早落库。界面上看到记录的时间会比实际检测稍晚；记录里的时间戳是检测到事件的时间。

调试日志：

```bash
CLIPBOARD_MONITOR_LOG=debug ./target/release/clipboard-monitor serve
```

调试日志不打印剪贴板正文。

## 限制

- 只能看到 X11 协议里的剪贴板访问。程序如果在自己的进程里缓存了旧内容，不再向 X 服务器请求，就不会产生读取记录。
- 所有者如果在内容被取走前退出，对应写入会留下“没有取到内容”的说明。
- 一次粘贴可能对多个格式各请求一次。300ms 内同一程序对同一选择区的读取会合并成一条。
- 有的桌面组件会每隔大约一秒读一次剪贴板。这些读取都会留下记录，旧记录由保留天数和容量上限清掉。
- RECORD 上下文挂在监控进程上。监控停止后不再记录。
