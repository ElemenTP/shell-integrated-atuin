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
│  atuin_module.c (C shim)      │  │  AtuinNative/ (.NET 8)          │
│  → atuin_native zsh module    │  │  NativeMethods.cs (LibraryImport)│
│  链接 libatuin_ffi cdylib     │  │  Session.cs (托管封装)           │
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
| `crates/atuin/src/command/client/search.rs` | 抽出 `run_non_interactive_with_context`；新增 `in_process_event` 模块声明 |
| `crates/atuin/src/command/client/search/interactive.rs` | 输入层在 `in-process` 下切换到安全事件源，其余上游 TUI 状态机/绘制完全复用 |
| `crates/atuin-client/src/settings.rs` | meta store 改为可替换的 `Arc<MetaStore>` 全局缓存，新增 `close_meta_store()` |
| `crates/atuin-client/src/meta.rs` | 新增 `MetaStore::close()`，关闭 meta.db 的 sqlx worker |
| `crates/atuin-client/src/database.rs` | `Sqlite::close()` 改为公开方法，供卸载路径等待 sqlx worker |
| `crates/atuin-client/src/record/sqlite_store.rs` | 新增 `SqliteStore::close()`，关闭 records.db 的 sqlx worker |

`Session::new(data_dir: Option<&Path>)` 内部：

- 使用 `tokio::runtime::Builder::new_multi_thread().worker_threads(2)`，让
  fire-and-forget 的 `history_end` 真正在后台 worker 上执行
- 打开 `history.db` 与 `records.db` 并持有连接池
- 加载/生成 PASETO v4 key 并缓存
- `history_start` 直接写入 history.db，与官方 `atuin history start` 相同
- `history_end` 更新 history.db 并追加加密 record 到 records.db
- `search_prefix` / `search(mode, …)` 在当前 session 的数据库上做
  Prefix / FullText / Fuzzy 搜索，TUI 按按键实时查询
- `Drop` 调用 `shutdown_timeout`，等待后台任务完成后才允许 `dlclose`

### 2. FFI API 设计

```c
// 生命周期
atuin_session_t *atuin_session_create(const char *data_dir);
void             atuin_session_destroy(atuin_session_t *s);

// 历史记录
int  atuin_history_start(atuin_session_t *s, const char *command, const char *cwd,
                          const char *author, const char *intent, char **id_out);
int  atuin_history_end(atuin_session_t *s, const char *id,
                        int64_t exit_code, int64_t duration_ns, int sync);
int  atuin_search_prefix(atuin_session_t *s, const char *query, int limit,
                          char **out);
/* 0=选中（*out 可释放，可能带 __atuin_accept__: 前缀），1=取消，<0=错误 */
int  atuin_search_interactive(atuin_session_t *s, const char *query,
                               char **out);

// 内存/元数据
void        atuin_free_string(char *ptr);
const char *atuin_session_uuid(atuin_session_t *s);
const char *atuin_version(void);
const char *atuin_last_error(void);
```

**内存所有权**：

| 函数 | 返回字符串 | 释放方式 |
| --- | --- | --- |
| `atuin_history_start` → `id_out` | Rust 分配 | `atuin_free_string` |
| `atuin_search_prefix` → `out` | Rust 分配 | `atuin_free_string` |
| `atuin_search_interactive` → `out` | Rust 分配（取消时为 NULL） | `atuin_free_string` |
| `atuin_session_uuid` | session 持有 | 不可释放 |
| `atuin_version` | 静态字符串 | 不可释放 |
| `atuin_last_error` | 库持有，下次调用前有效 | 不可释放 |

失败时 `id_out` / `out` 会先被重置为 NULL，避免调用方持有上一次成功调用的悬垂指针。

### 3. 进程安全机制

| 机制 | 目的 |
| --- | --- |
| Fork guard（`creator_pid`） | zsh 的 `$()`、`&`、管道非末位、子 shell 会 fork 不 exec；子进程继承损坏的 tokio runtime。FFI 检测 PID 变化并返回错误 |
| multi-thread tokio runtime + 显式 SQLite pool close | `history_end` 的 fire-and-forget 在 worker 线程执行，precmd builtin 不再阻塞；卸载时依次等待 tokio worker、history/records/meta 三个 sqlx pool 的 worker 退出 |
| 全局 `Mutex<Option<CString>>` 错误槽 | 避免 TLS 析构器在宿主线程上悬挂（macOS/Windows 无 DSO 卸载保护） |
| `catch_unwind` panic 隔离 | 阻止 Rust panic 跨过 C ABI 边界 |
| 输出指针先置 NULL | 失败调用不会留下 stale pointer |

### 4. zsh 集成契约

zsh 模块提供 6 个 builtin。**关键协议**：所有可能被 hook 使用的返回值同时写入 zsh 参数，
插件绝不使用 `$(...)`（命令替换会 fork）：

| Builtin | 写入参数 | stdout | 对应官方命令 |
| --- | --- | --- | --- |
| `atuin_history_start cmd [cwd]` | `ATUIN_HISTORY_ID` | ID | `atuin history start --hook` |
| `atuin_history_end id exit [duration] [--sync]` | — | — | `atuin history end &` |
| `atuin_search query [limit]` | `ATUIN_SEARCH_RESULT` | 多行结果 | `atuin search --cmd-only` |
| `atuin_search_interactive [query]` | `ATUIN_SEARCH_SELECTED` | —（全屏 TUI） | `atuin search -i` |
| `atuin_session_id` | `ATUIN_SESSION` | UUID | `atuin uuid` |
| `atuin_version` | `ATUIN_NATIVE_VERSION` | 版本号 | — |

`atuin-native.plugin.zsh` 复刻官方 `atuin.zsh`：

- `ATUIN_SESSION` / `ATUIN_SHLVL` 会话初始化
- preexec / precmd / zshaddhistory 三个 hook（preexec 内同时打点计时）
- zsh-autosuggestions 的 `atuin_native` strategy
- `atuin-search` / `atuin-up-search` 等 ZLE widget 兼容名（widget 调用
  `atuin_search_interactive`，选中后写入 `LBUFFER`；`__atuin_accept__:`
  前缀会触发 `zle accept-line`）
- 与 `atuin init zsh` 相同的默认键位：emacs/viins 的 Ctrl+R、vicmd 的 `/`、
  三种 keymap 的 UpArrow、vicmd 的 `k` 都自动绑定到对应 widget
- OSC 133 标记（`ATUIN_PTY_PROXY_ACTIVE` 开启时）
- 标准 `*.plugin.zsh` 入口 + `ATUIN_NATIVE_DIR` 模块目录发现

### 5. pwsh 集成契约

- `AtuinNative.dll` 使用 .NET 7+ `LibraryImport` 源码生成器 P/Invoke
- `NativeMethods` 注册 `DllImportResolver`：优先 `ATUIN_FFI_PATH`，否则探测程序集同目录
- `AtuinSession` 是 `IDisposable` 的托管封装，`Dispose` 触发 native session 析构
- `AtuinEnvironment` 同时写 .NET 与 libc `setenv` 环境块，保证内嵌 Rust 能通过 `std::env` 读取
- `atuin-native.psm1` 复刻官方 `atuin.ps1` 的 `PSConsoleHostReadLine` 方案：
  读下一行前 finalize 上一条命令，读到后 start 新命令
- `Invoke-AtuinSearch` / `Enable-AtuinSearchKeys` 调用进程内全屏 TUI，
  选中后替换命令行；`__atuin_accept__:` 前缀触发 `AcceptLine`
- 模块导入时自动执行 `Enable-AtuinSearchKeys -CtrlR $true -UpArrow $true`，
  与 `atuin init powershell` 的默认行为一致
- 模块移除时恢复原始 `PSConsoleHostReadLine` 并 Dispose native session

### 6. 已知边界

- shell 集成路径（history 记录、prefix search、交互式 TUI 搜索）全部是进程内的；
  **daemon 模式和网络 sync 仍由官方 `atuin` 二进制完成**
- 交互式 TUI 直接复用上游 `search/interactive.rs` 的完整状态机与绘制
  （tabs/inspector/预览/键位配置等）；仅在 Unix 进程内把事件源替换为
  `in_process_event.rs`，避免 `crossterm::event` 的 SIGWINCH 回调在
  `dlclose` 后悬挂。上游支持的功能因此同步继承
- zsh 模块构建依赖已 `configure` 的 zsh 源码树（仅头文件）
- PowerShell 模块目标框架为 `net8.0`，需要 .NET SDK 8+（或 roll-forward 环境）

## 目录结构

```
shell-integrated-atuin/
├── rust_src/                     # FFI crate (cdylib)
│   ├── Cargo.toml
│   └── src/lib.rs, ffi.rs        # C API + 19 个 Rust 单元测试
├── zsh_src/                      # zsh 模块
│   ├── atuin_ffi.h               # C 头文件
│   ├── atuin_module.c            # zsh shim（5 个 builtin）
│   ├── atuin_builtin_util.{h,c}  # 无 zsh 依赖的参数解析（可单测）
│   ├── atuin-native.plugin.zsh   # zsh 插件入口
│   └── build/                    # CMake 输出（Debug/Release 子目录）
├── pwsh_src/                     # PowerShell 模块
│   ├── AtuinNative/              # C# 二进制模块
│   ├── AtuinNative.Tests/        # 无外部依赖的托管单元测试 runner
│   ├── atuin-native.psm1         # PSReadLine/history/search 集成
│   └── atuin-native.psd1         # 模块 manifest
├── tests/
│   ├── ffi_smoke.c               # dlopen 系统测试（含 fork guard）
│   ├── test_zsh_module_unit.c    # zsh builtin 参数解析单元测试
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
