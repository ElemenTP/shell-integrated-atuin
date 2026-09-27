# 架构设计：Atuin 作为 Shell 原生动态库

## 问题背景

Atuin 的 shell 集成在每个命令生命周期内需要多次启动独立进程：

```
传统模式（每命令 2+ 次 fork + exec）:
  preexec:  ATUIN_HISTORY_ID=$(atuin history start --hook -- "$1")   # 1 次进程
  precmd:   (atuin history end --hook ... &)                          # 1 次进程
  自动补全:  atuin search --cmd-only --limit 1 ...                    # 每次按键
  交互搜索:  atuin search -i ...                                      # 每次 Ctrl-R
```

每次启动都要重新解析配置、打开 SQLite、加载加密 key、初始化 tokio runtime。
这些状态本来可以在 shell 生命周期内复用。

## 解决方案

把 Atuin 的 shell 集成路径编译为 C ABI 动态库，由 zsh/pwsh 进程内直接调用：

- **零进程创建**：`history start/end`、prefix search、交互式全屏 TUI 全部在 shell 进程内完成
- **状态持久化**：`Session` 在 shell 生命周期内持有 SQLite 连接、Settings、加密 key、tokio runtime
- **进程级单 session**：Atuin client 的数据目录/meta store 是进程级全局状态，FFI 因此只维护一个 `Session`（无 handle、单锁串行化）
- **fork 安全**：FFI 层拒绝在 `$()`、`&`、管道等 fork 子进程中触碰 tokio runtime
- **可卸载**：session 析构时关闭 multi-thread tokio runtime 并等待 worker 退出，`dlclose` 后无悬挂线程/TLS 析构器

## 整体架构

```
┌───────────────────────────────────────────────────────────────────┐
│  rust_src/  (atuin-ffi crate, cdylib)                             │
│  ┌─────────────────────────────────────────────────────────────┐  │
│  │  ffi.rs — extern "C" atuin_session_* / atuin_history_* API  │  │
│  │  参数转换、panic 隔离、fork guard、输出内存所有权管理          │  │
│  └──────────────────────────┬──────────────────────────────────┘  │
│                             │ path dependency, features=["client","in-process"]
│  ┌──────────────────────────▼──────────────────────────────────┐  │
│  │  ../atuin/                                                   │  │
│  │  crates/atuin/src/session.rs                                │  │
│  │  Session — Settings / Sqlite / HistoryStore / Key 常驻缓存   │  │
│  │  + 复用上游 history/search/交互式 TUI 命令实现               │  │
│  │  crates/atuin/src/command/client/search/in_process_event.rs │  │
│  │  仅 Unix 进程内输入：替代 crossterm::event 避免 SIGWINCH     │  │
│  └─────────────────────────────────────────────────────────────┘  │
└─────────────────────┬──────────────────────┬──────────────────────┘
                      │ libatuin_ffi.{so,dylib,dll}
                      ▼                       ▼
┌───────────────────────────────┐  ┌─────────────────────────────────┐
│  zsh_src/                     │  │  pwsh_src/                      │
│  module.c (C shim)      │  │  AtuinNative/ (.NET 8)          │
│  → atuin_native zsh module    │  │  NativeMethods.cs (LibraryImport)│
│  链接 libatuin_ffi cdylib     │  │  Session.cs (静态封装)           │
│  内置命令:                    │  │  Environment.cs (双环境块写入)   │
│    atuin_history_start        │  │  atuin-native.psm1 (shell 逻辑) │
│    atuin_history_end          │  │  atuin-native.psd1 (manifest)   │
│    atuin_search               │  │  加载方式: Import-Module        │
│    atuin_session_id           │  │  平台: Windows/Linux/macOS      │
│    atuin_version              │  │                                 │
│  加载方式: zmodload           │  │                                 │
│  平台: Linux / macOS          │  │                                 │
└───────────────────────────────┘  └─────────────────────────────────┘
```

## 核心设计决策

### 1. 特性门控修改 Atuin

Atuin 源码通过 `#[cfg(feature = "in-process")]` 做最小化加法修改，
`rust_src` 通过 path dependency 引用（`atuin -> ../atuin` 符号链接）。

**Atuin 修改清单**：

| 文件 | 修改 |
| --- | --- |
| `crates/atuin/Cargo.toml` | 新增 `in-process` feature（启用 `client` 与 `atuin-client/in-process`） |
| `crates/atuin/src/lib.rs` | **新建** — 使 `atuin` 同时可作为 library 使用 |
| `crates/atuin/src/session.rs` | **新建** — 进程内 Session；history/end/search 直接委托上游 `command::client::*` |
| `crates/atuin/src/command/client/history.rs` | 抽出 `handle_start_with_cwd` / `handle_end`，供 Session 复用 |
| `crates/atuin/src/command/client/search.rs` | 抽出 `run_non_interactive_with_context`；新增 `in_process_event` 模块声明与 `reset_tui_input()` |
| `crates/atuin/src/command/client/search/interactive.rs` | 输入层在 `in-process` 下切换到安全事件源，其余上游 TUI 状态机/绘制完全复用 |
| `crates/atuin/src/command/client/search/in_process_event.rs` | 新增 `reset()`：释放缓存的终端句柄与残留输入 |
| `crates/atuin-client/src/settings.rs` | meta store 改为可替换的 `Arc<MetaStore>` 全局缓存；`DATA_DIR`/`META_CONFIG` 改为可清空的 `RwLock<Option<_>>`；新增 `close_meta_store()` 与 `shutdown_process_state()` |
| `crates/atuin-client/src/meta.rs` | 新增 `MetaStore::close()`，关闭 meta.db 的 sqlx worker |
| `crates/atuin-client/src/database.rs` | `Sqlite::close()` 改为公开方法，供卸载路径等待 sqlx worker |
| `crates/atuin-client/src/record/sqlite_store.rs` | 新增 `SqliteStore::close()`，关闭 records.db 的 sqlx worker |

`Session::new()`（无参数，数据目录由配置解析）与
`Session::new_with_datadir(Option<&Path>)`（显式目录，保留给测试）内部：

- 使用 `tokio::runtime::Builder::new_multi_thread().worker_threads(8)`，让
  fire-and-forget 的 `history_end` 真正在后台 worker 上执行
- 打开 `history.db` 与 `records.db` 并持有连接池
- 加载/生成 PASETO v4 key 并缓存
- `history_start` 直接写入 history.db，与官方 `atuin history start` 相同
- `history_end` 更新 history.db 并追加加密 record 到 records.db
- `search(SearchOptions)` 是统一入口：按 `SearchMode` 分派到上游普通搜索或
  交互式 TUI；`search_prefix` / `search_interactive` 等是调用它的快速路径
- `Drop` 先释放 TUI 缓存的终端输入句柄，再在 tokio runtime 内 join 在途任务、
  关闭 SQLite pools 与 meta store、清空 `DATA_DIR`/`META_CONFIG` 缓存，最后
  `shutdown_timeout` 后允许 `dlclose`

### 2. FFI API 设计

```c
/* 错误协议：所有可能失败的导出都返回 char *。
 * NULL = 成功；非 NULL = Rust 分配的 UTF-8 错误信息，调用方用 atuin_free 释放。 */

// 生命周期：进程级单 session，无 handle 参数。
// 数据目录与官方 CLI 一样由配置解析（ATUIN_DATA_DIR / XDG / config.toml 的 data_dir）。
// init 幂等（已有 session 时成功并保留它）；shutdown 幂等；其余导出在无 session 时报错。
char *atuin_init(void);
char *atuin_shutdown(void);

// 历史记录
// author_kind 为 "user"/"agent"（大小写不敏感），对应 `atuin history start --author-kind`
char *atuin_history_start(const char *command, const char *cwd,
                          const char *author, const char *author_kind,
                          const char *intent, char **id_out);
char *atuin_history_end(const char *id,
                        int64_t exit_code, int64_t duration_ns, int sync);
/* atuin_search_options_t 的 exits/exclude_exits 是 int64 数组
 * （对应上游可重复的 --exit / --exclude-exit），空数组表示不过滤。 */
char *atuin_search(const atuin_search_options_t *options, char **out);
char *atuin_search_prefix(const char *query, int limit, char **out);
/* 成功返回 NULL；*out != NULL 表示选中（可带 __atuin_accept__: 前缀），
 * *out == NULL 表示用户取消。keymap_mode 为 ATUIN_KEYMAP_MODE_* 之一。 */
char *atuin_search_interactive(const char *query, int shell_up_key_binding,
                               int keymap_mode, char **out);

// 会话统计快照
typedef struct atuin_stats {
    uint64_t history_starts, history_ends_sync, history_ends_async;
    uint64_t search_calls, search_prefix_calls;
    uint64_t interactive_search_calls, interactive_selections, interactive_cancels;
    uint64_t in_flight_history_ends, uptime_secs;
} atuin_stats_t;
char *atuin_stats(atuin_stats_t *out);

// 内存/元数据
void  atuin_free(char *ptr);
char *atuin_session_uuid(const char **out);
const char *atuin_version(void);
```

`atuin_search_interactive` 按参数选择 Session 的快捷路径：显式 `keymap_mode` 用
`search_interactive_with`，仅设置 up 标志用 `search_interactive_up`，否则用
`search_interactive`；`atuin_search_prefix` 对应 `Session::search_prefix`。

**内存所有权**：

| 函数 | 返回字符串 | 释放方式 |
| --- | --- | --- |
| `atuin_history_start` → `id_out` | Rust 分配 | `atuin_free` |
| `atuin_search` / `atuin_search_prefix` → `out` | Rust 分配 | `atuin_free` |
| `atuin_search_interactive` → `out` | Rust 分配（取消时为 NULL） | `atuin_free` |
| 任意失败调用的返回值 | Rust 分配的错误信息 | `atuin_free` |
| `atuin_session_uuid` → `*out` | session 持有 | 不可释放 |
| `atuin_stats` → `*out` | 纯值快照（无指针） | 不可释放 |
| `atuin_version` | 静态字符串 | 不可释放 |

失败时输出参数（`id_out` / `out` / `uuid out`）会先被重置为
NULL，避免调用方持有上一次成功调用的悬垂指针。错误信息随返回值返回，不再有
进程级错误槽，因此单次调用之间互不影响。

### 3. 进程安全机制

| 机制 | 目的 |
| --- | --- |
| 进程级单 session（`Mutex<Option<Session>>`） | Atuin client 层的数据目录/meta store 是进程级全局状态，只对一个 session 成立。FFI 在库内串行化所有调用，第二个 create 报错 |
| 调用锁 | `search_interactive` 在 TUI 期间持锁；`history_end(sync=0)` 的 spawned future 只用克隆的 Arc，调用返回即释放锁 |
| Fork guard（`creator_pid`） | zsh 的 `$()`、`&`、管道非末位、子 shell 会 fork 不 exec；子进程继承损坏的 tokio runtime。FFI 检测 PID 变化并返回错误 |
| multi-thread tokio runtime + 显式 SQLite pool close | `history_end` 的 fire-and-forget 在 worker 线程执行，precmd builtin 不再阻塞；destroy 时在 runtime 内 join 在途任务并关闭 history/records/meta 三个 sqlx pool，再关闭 tokio runtime |
| destroy 时重置进程级缓存 | `Settings::shutdown_process_state()` 清空 `DATA_DIR` / `META_CONFIG`（否则 destroy→create 会沿用上一次的目录，导致 history/records 与 meta.db 分裂）；`reset_tui_input()` 释放 TUI 缓存的终端句柄与残留输入 |
| 返回值错误协议（无全局错误槽） | 错误字符串随每次调用返回并由调用方释放，没有共享可变状态、没有 TLS/全局析构器，可在 `dlclose` 后安全卸载 |
| `catch_unwind` panic 隔离 | 阻止 Rust panic 跨过 C ABI 边界；锁中毒通过 `into_inner()` 恢复 |
| 输出指针先置 NULL | 失败调用不会留下 stale pointer |

### 4. zsh 集成契约

zsh 模块提供 8 个 builtin，全部为**零参数 builtin**。输入与输出通过 zsh 参数
交换（与 starship/zoxide native 模块一致），C shim 不解析 argv；插件绝不使用
`$(...)`（命令替换会 fork）：

| Builtin | 输入参数 | 输出参数 | 对应官方命令 |
| --- | --- | --- | --- |
| `atuin_history_start` | `ATUIN_HISTORY_COMMAND`, `ATUIN_HISTORY_CWD`, `ATUIN_HISTORY_AUTHOR`, `ATUIN_HISTORY_AUTHOR_KIND`, `ATUIN_HISTORY_INTENT` | `ATUIN_HISTORY_ID` | `atuin history start --hook` |
| `atuin_history_end` | `ATUIN_HISTORY_ID`, `ATUIN_HISTORY_EXIT`, `ATUIN_HISTORY_DURATION_NS`, `ATUIN_HISTORY_SYNC` | — | `atuin history end &` |
| `atuin_search` | `ATUIN_SEARCH_QUERY`, `ATUIN_SEARCH_MODE`, `ATUIN_SEARCH_FILTER_MODE`, `ATUIN_SEARCH_CWD`, `ATUIN_SEARCH_EXCLUDE_CWD`, `ATUIN_SEARCH_EXITS`, `ATUIN_SEARCH_EXCLUDE_EXITS`, `ATUIN_SEARCH_BEFORE`, `ATUIN_SEARCH_AFTER`, `ATUIN_SEARCH_LIMIT`, `ATUIN_SEARCH_OFFSET`, `ATUIN_SEARCH_REVERSE`, `ATUIN_SEARCH_INCLUDE_DUPLICATES`, `ATUIN_SEARCH_AUTHORS`, `ATUIN_SEARCH_SHELLS` | `ATUIN_SEARCH_RESULT` | `atuin search` 非交互选项 |
| `atuin_search_prefix` | `ATUIN_SEARCH_QUERY`, `ATUIN_SEARCH_LIMIT`（默认 1） | `ATUIN_SEARCH_RESULT` | `atuin search --cmd-only --author '$all-user' --search-mode prefix` |
| `atuin_search_interactive` | `ATUIN_SEARCH_QUERY`, `ATUIN_SEARCH_SHELL_UP_KEY_BINDING`, `ATUIN_SEARCH_KEYMAP_MODE` | `ATUIN_SEARCH_SELECTED` | `atuin search -i [--shell-up-key-binding] [--keymap-mode=…]` |
| `atuin_stats` | `ATUIN_STATS_VERBOSE`, `ATUIN_STATS_QUIET` | `ATUIN_STATS_*` | `starship_stats` 风格的会话统计 |
| `atuin_session_id` | — | `ATUIN_SESSION` | `atuin uuid` |
| `atuin_version` | — | `ATUIN_VERSION` | — |

`atuin-native.plugin.zsh` 复刻官方 `atuin.zsh`：

- `ATUIN_SESSION` / `ATUIN_SHLVL` 会话初始化
- preexec / precmd / zshaddhistory 三个 hook（preexec 内同时打点计时）
- zsh-autosuggestions 的 `atuin` strategy（与官方同名，`atuin_native` 为兼容别名）
  调用 `atuin_search_prefix`（官方 `--cmd-only --author '$all-user' --limit 1
  --search-mode prefix` 的专用快路径）。与官方一样**无条件**定义 strategy 并把
  `atuin` 前插到 `ZSH_AUTOSUGGEST_STRATEGY`，因此在 zsh-autosuggestions 之前
  source 也生效
- `atuin-search` / `atuin-up-search` 等 ZLE widget 兼容名（widget 调用
  `atuin_search_interactive`，选中后写入 `LBUFFER`；`__atuin_accept__:`
  前缀会触发 `zle accept-line`）
- UpArrow widget 设置 `ATUIN_SEARCH_SHELL_UP_KEY_BINDING=1`，vi widget 设置
  `ATUIN_SEARCH_KEYMAP_MODE=vim-normal|vim-insert`，与官方 `--shell-up-key-binding`
  / `--keymap-mode` 对齐
- 与 `atuin init zsh` 相同的默认键位：emacs/viins 的 Ctrl+R、vicmd 的 `/`、
  三种 keymap 的 UpArrow、vicmd 的 `k` 都自动绑定到对应 widget；设置
  `ATUIN_NOBIND` 时跳过绑定（同官方 `init`）
- OSC 133 标记：使用官方同款 `__atuin_pty_proxy_owns_tty` 契约与
  `\033]133;D;<exit>;history_id=<id>\a` 格式
- 标准 `*.plugin.zsh` 入口 + `ATUIN_NATIVE_DIR` 模块目录发现
- `export ATUIN_TMUX_POPUP=false`：进程内 TUI 直接在当前终端绘制

进程内无法提供、因此**有意不支持**的官方功能（都需要外部 `atuin` 二进制或
daemon）：tmux popup 搜索、`atuin ai inline` 自然语言模式、`__internal
prepare-search-index` 索引预热；PTY proxy 的存活性探测以 `ATUIN_PTY_PROXY_ACTIVE`
近似（proxy preamble 预设的 `__atuin_pty_proxy_owns_tty` 优先）。

### 5. pwsh 集成契约

- `AtuinNative.dll` 使用 .NET 7+ `LibraryImport` 源码生成器 P/Invoke
- `NativeMethods` 注册 `DllImportResolver`：优先 `ATUIN_FFI_PATH`，否则探测程序集同目录
- `AtuinSession` 是无实例的静态封装（native 只有一个进程级 session）：
  `Init()` 创建，`Shutdown()` 幂等析构，`HistoryStart`/`HistoryEnd`/`Search*`/
  `SessionUuid`/`GetStats` 都是静态方法；重复 `Init()` 幂等并保留现有 session
- `HistoryStart(command, cwd, author, authorKind, intent)` 对应官方
  `--author` / `--author-kind`（`AtuinAuthorKind.User`/`Agent`）/ `--intent`；
  `AtuinSearchOptions.Exits` / `ExcludeExits`（`long[]`）对应可重复的
  `--exit` / `--exclude-exit`
- `AtuinEnvironment` 同时写 .NET 与 libc `setenv` 环境块，保证内嵌 Rust 能通过 `std::env` 读取
- `atuin-native.psm1` 复刻官方 `atuin.ps1` 的 `PSConsoleHostReadLine` 方案：
  读下一行前 finalize 上一条命令，读到后 start 新命令；首次使用时
  `Initialize-AtuinNativeSession` 调用 `[AtuinNative.Session]::Initialize()`
- `Invoke-AtuinSearch` / `Enable-AtuinSearchKeys` 调用进程内全屏 TUI，
  选中后替换命令行；`__atuin_accept__:` 前缀触发 `AcceptLine`；
  `-ExtraArgs` 里的 `--shell-up-key-binding` / `--keymap-mode=…` 会转成
  `SearchInteractive` 的 up/vi 参数（官方 UpArrow / vi widget 行为）
- `Get-AtuinNativeStats` 返回 `[AtuinNative.Session]::GetStatsReport()` 的会话统计摘要
- 模块导入时自动执行 `Enable-AtuinSearchKeys -CtrlR $true -UpArrow $true`，
  与 `atuin init powershell` 的默认行为一致
- 模块移除时恢复原始 `PSConsoleHostReadLine` 并调用 `[AtuinNative.Session]::Shutdown()`

### 6. 已知边界

- shell 集成路径（history 记录、prefix search、交互式 TUI 搜索）全部是进程内的；
  **daemon 模式和网络 sync 仍由官方 `atuin` 二进制完成**
- 交互式 TUI 直接复用上游 `search/interactive.rs` 的完整状态机与绘制
  （tabs/inspector/预览/键位配置等）；仅在 Unix 进程内把事件源替换为
  `in_process_event.rs`，避免 `crossterm::event` 的 SIGWINCH 回调在
  `dlclose` 后悬挂。该事件源同时解析按键（含 CSI-u / Kitty keyboard
  protocol）、SGR/X10 鼠标报告（滚轮选择）与括号粘贴，因此上游 TUI 启用的
  终端模式不会因未识别序列而被误判成 Esc。上游支持的功能因此同步继承
- zsh 模块构建依赖已 `configure` 的 zsh 源码树（仅头文件）
- PowerShell 模块目标框架为 `net8.0`，需要 .NET SDK 8+（或 roll-forward 环境）

## 目录结构

```
shell-integrated-atuin/
├── rust_src/                     # FFI crate (cdylib)
│   ├── Cargo.toml
│   └── src/lib.rs, ffi.rs        # C API + 19 个 Rust 单元测试
├── zsh_src/                      # zsh 模块
│   ├── ffi.h               # C 头文件
│   ├── module.c            # zsh shim（6 个零参数 builtin，变量传参）
│   ├── atuin-native.plugin.zsh   # zsh 插件入口
│   └── build/                    # CMake 输出（Debug/Release 子目录）
├── pwsh_src/                     # PowerShell 模块
│   ├── AtuinNative/              # C# 二进制模块
│   ├── AtuinNative.Tests/        # 无外部依赖的托管单元测试 runner
│   ├── atuin-native.psm1         # PSReadLine/history/search 集成
│   └── atuin-native.psd1         # 模块 manifest
├── tests/
│   ├── ffi_smoke.c               # dlopen 系统测试（含 fork guard）
│   ├── test_zsh.sh               # zsh 模块集成测试
│   ├── test_fork_zsh.sh          # fork 安全回归测试
│   ├── test_unload_zsh.sh        # load/unload 循环与线程泄漏测试
│   ├── test_plugin_zsh.sh        # 插件 hook 协议集成测试
│   ├── test_tui_zsh.py           # zsh TUI pty 集成测试（真键盘输入）
│   ├── test_pwsh.ps1             # pwsh 模块集成测试
│   └── test_tui_pwsh.py          # pwsh TUI pty 集成测试（Ctrl+R handler）
├── docs/
│   ├── architecture.md           # 本文档
│   ├── implementation-notes.md   # 实现细节与踩坑记录
│   └── testing.md                # 测试矩阵与运行方法
├── atuin-native.plugin.zsh       # 根目录插件入口（符号链接，插件管理器约定）
├── CMakeLists.txt
└── README.md
```

## 构建系统

CMake 统一编排 Cargo、zsh shim、dotnet 和测试：

1. **Atuin 源码**：`ATUIN_SOURCE=AUTO` 从 GitHub clone `in-process` 分支，或指定本地路径
2. **zsh 源码**：`ZSH_SOURCE=AUTO` 下载 zsh 5.9.2 并自动 `./configure` + 生成 `zsh.mdh`
3. **Rust FFI**：`add_custom_command(OUTPUT libatuin_ffi...)` 提供真正的增量构建
4. **zsh 模块**：`BUILD_RPATH` 指向 cargo target，`INSTALL_RPATH` 使用 `$ORIGIN` / `@loader_path`
5. **pwsh 模块**：dotnet build 后把 FFI 库和 manifest 复制到同一目录
6. **测试**：CTest 管 C 冒烟/单元测试；`test-*` custom target 管各语言集成测试

详见 README 与 `docs/testing.md`。
