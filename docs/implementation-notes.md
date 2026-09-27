# 实现笔记

本文档记录把 Atuin 改造为 shell 进程内插件过程中的关键实现细节与踩坑记录。

## 1. Atuin 内部改造

### 1.1 常驻 Session 使用 multi-thread tokio runtime

`crates/atuin/src/session.rs` 持有一个多线程 `Runtime`：

```rust
let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(8)
    .enable_all()
    .build()?;
```

选择 multi-thread 的原因：shell precmd 中的 `atuin_history_end` 必须等价于
官方的 `(atuin history end ... &)`。如果使用 current-thread runtime，
`runtime.spawn()` 只是把任务放进本地队列；builtin 返回 zsh 后没有
`block_on` 继续驱动 runtime，任务要等下一次 FFI 调用才真正执行——不是真正的
fire-and-forget。多线程 runtime 的 worker 会在 builtin 返回后立即消费任务。

对“worker 线程污染 zsh 主线程 / 难以卸载”的应对：

1. **不保留全局 TLS，也不保留全局错误槽**：FFI crate 不使用 `thread_local!`，
   错误直接作为返回值（`char *`，NULL = 成功）返回给调用方
2. **fork guard**：`$()`、`&`、管道等 fork 子进程在触碰 runtime 前被拒绝
3. **显式 shutdown**：`Session::drop` 在 Session 自己的 tokio runtime 内
   等待在途 history_end 计数归零，再关闭 SQLite pool，最后才 `shutdown_timeout(10s)`
4. **线程数受控**：`worker_threads(8)`，一个 shell 只常驻少量 worker

`Session::drop` 的调用链：

```text
zmodload -u / Remove-Module
  → cleanup_ / OnRemove
  → atuin_shutdown
  → Session::drop
  → runtime.block_on:
       等待 in-flight history_end 计数器归零（不保存 JoinHandle 列表）
       history.db pool close           # 等待 sqlx worker
       records.db pool close           # 等待 sqlx worker
       Settings::close_meta_store()    # 等待 meta.db 的 sqlx worker
  → Runtime::shutdown_timeout(10s)     # 等待 tokio worker 退出
  → dlclose
```

这里刻意**不再使用 `futures::executor::block_on`**：关闭数据库的动作全部在
Session 自己的 tokio runtime 内完成，避免为卸载路径引入额外 executor /
thread-local 状态。同时使用 `AtomicUsize + tokio::sync::Notify` 只记录在途
`history_end_async` 数量，而不是保存每个 `JoinHandle`，避免长生命周期 shell
中 `pending` 集合无限增长。

sqlx SQLite 连接不是 tokio task，而是自己的 `sqlx-sqlite-worker-*` 线程。
仅关闭 tokio runtime 后 zsh 仍会残留 3 个 sqlx 线程（history / records /
meta 三个 pool）。`Session::shutdown` 必须逐个 `pool.close().await`。
meta store 由 `Settings` 的进程级 `OnceCell` 持有，无法原地重建；为支持
`zmodload -u` 后重新 `zmodload`，其全局缓存改为可替换的
`OnceLock<Mutex<Option<Arc<MetaStore>>>>`，`close_meta_store()` 取出旧 store
关闭，下一次 `meta_store()` 再建新 pool。

### 1.2 Settings::new 与 meta store 的隐式依赖

`Session` 最初使用：

```rust
Settings::builder()?.build()?.try_deserialize()?
```

这**不会**注册 `Settings::host_id()` 需要的全局 meta store 配置。
在 `history_end` 需要把加密 record 写入 records.db 时，第一次调用 `Settings::host_id()`
会报 `meta store config not set — Settings::new() has not been called`。

正确写法是直接调用 `Settings::new()?`，它内部执行同样的配置解析并注册 meta store。

`Session::new()` 不接受 data-dir 参数，完全依赖 `Settings::new()` 解析出的
`settings.db_path` / `record_store_path` / `key_path`，因此 `ATUIN_DATA_DIR`、
XDG 与 config.toml 中的 `data_dir` 都按官方优先级生效，FFI 也无法覆盖它。
底层 `Session::new_with_datadir(Option<&Path>)` 保留下来：FFI 内的私有辅助函数
`atuin_init_with_datadir` 在测试里显式传目录，让每个单元测试拥有独立
数据目录；导出符号 `atuin_init` 只是以 `NULL` 调用它，所以 shell 侧
始终跟随配置。

因为 `DATA_DIR` / `META_CONFIG` / `META_STORE` 都是进程级全局状态，FFI 只维护
**一个进程级 session**（`static SESSION: Mutex<Option<SessionHandle>>`，命名与
starship wrapper 的 `ssp_init` / `ssp_shutdown` 对齐）：`atuin_init` 幂等
（已有 session 时成功且保留原 session）、`atuin_shutdown` 幂等、其余导出在无
session 时报 `session is not initialized`。`SESSION_PID` 原子变量放在锁之外，
fork 子进程无需先拿锁即可被拒绝。
所有调用由同一把锁串行化；`history_end(sync=0)` 的 spawned future 只捕获克隆的
Arc，因此调用返回即释放锁，而 `search_interactive` 在 TUI 期间持锁（等价官方
前台进程）。这也是为什么之前为多 session 加的 `LIVE_SESSIONS` 计数被移除：
单 session 下 `Drop` 可以无条件关闭 meta store。

### 1.2.1 shutdown → init 与进程级静态变量

单 session 允许 `atuin_shutdown()` 后重新 `atuin_init()`。为避免第二次 init 复用到上一次进程状态，
`Session::Drop` 调用 `Settings::shutdown_process_state()`：

- `close_meta_store()` 关闭 meta.db 的 sqlx worker 并把全局槽置空，下一次
  `meta_store()` 重新打开连接池；
- `DATA_DIR` / `META_CONFIG` 从 `OnceLock` 改为可清空的 `RwLock<Option<_>>`
  （写入仍是“首次生效”，保持 CLI 行为），清空后下一次 `Settings::new()` 会按新的
  `ATUIN_DATA_DIR` / config.toml 重新解析。否则 history.db/records.db 用新目录、
  meta.db 却留在旧目录，形成分裂状态；
- `reset_tui_input()` 释放 `in_process_event` 里缓存 `/dev/tty` 的 `OnceLock`，
  避免上一个 session 的 fd 与残留输入泄漏到下一个 session 或 `dlclose` 之后。

其余静态变量（`theme` / `history::all_user_author_filter` 的 `LazyLock`、
`atuin_domain::ATUIN_VERSION`、`DEFAULT_*_URL`）都是不可变缓存，destroy→create 无影响；
`settings/watcher.rs::SETTINGS_WATCHER` 只被 daemon 使用，不在本集成路径。

### 1.3 history_end 的本地记录链

官方 `atuin history end` 的本地路径不只更新 `history.db`：

```rust
db.update(&h).await?;
history_store.push(h).await?; // 加密后写入 records.db
```

常驻 Session 在启动时创建 `HistoryStore` 并缓存 host id 与 key，
避免每条命令重新读取。网络 sync 与 daemon 分支仍由官方二进制承担。

### 1.4 feature 是纯加法的

- 默认构建不包含 `in-process`，官方行为不变
- `crates/atuin/src/lib.rs` 让 `atuin` 二进制 crate 同时可作为 library
  使用；`#[cfg(feature = "in-process")] pub mod session;` 只新增进程内会话
- `Session` 不再在 `atuin-client` 中重复实现 history/search/TUI，而是调用
  `crates/atuin/src/command/client/{history,search}/...` 中的上游命令代码，
  同步上游功能时只需要跟上 CLI 实现

## 2. Rust FFI 层

### 2.1 Rust 2024 的 unsafe attribute

Rust 2024 edition 中 `#[no_mangle]` 是 unsafe attribute：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn atuin_init() -> *mut c_char { ... }
```

### 2.2 fork guard

zsh 以下场景会 `fork()` 且不 exec：

| 场景 | 子进程运行 builtin? | fork guard |
| --- | --- | --- |
| `$(atuin_history_start ...)` | ✅ 危险 | 拒绝 |
| `atuin_history_start ... &` | ✅ 危险 | 拒绝 |
| `atuin_history_start ... \| cat` | ✅ 危险 | 拒绝 |
| `(atuin_history_start ...)` | ✅ 危险 | 拒绝 |
| `atuin_history_start ... > file` | ❌ 不 fork | 放行 |
| `cat \| atuin_history_start ...` | ❌ 不 fork（末位） | 放行 |

实现方式是在进程级 session 状态里记录创建时的 PID：

```rust
struct SessionHandle {
    session: Session,
    creator_pid: u32,
    uuid: CString,
}
```

所有会触碰 tokio runtime 的入口先比较 `std::process::id()`。
`atuin_shutdown` 同样带 guard：fork 子进程不会 drop 父进程的 session。

### 2.3 输出指针先置 NULL

C 调用方常常复用同一个 `char **` 槽位。失败时如果保留上次的指针，
调用方可能误释放已释放的内存。所有带 out 指针的函数在参数校验**之前**执行：

```rust
if !id_out.is_null() {
    unsafe { *id_out = ptr::null_mut(); }
}
```

### 2.4 session UUID 不分配

最初 `atuin_session_uuid` 每次调用 `CString::new(...).unwrap().into_raw()`，
每次查询泄漏一个 CString。现在 UUID 在 session 创建时缓存到 `SessionHandle.uuid`，
调用方通过 out 参数拿到 session 持有的指针：

```rust
char *atuin_session_uuid(const char **out) {
    ...
    unsafe { *out = state.uuid.as_ptr(); }
    ptr::null_mut() // 成功
}
```

返回指针由 session 持有，文档明确“不可释放”。

### 2.5 错误直接作为返回值，不使用全局错误槽

FFI 不再维护 `Mutex<Option<CString>>` 或 `atuin_last_error`。每个可能失败的导出
都返回 `char *`：

- `NULL` = 成功；
- 非 NULL = Rust 分配的 UTF-8 错误信息，调用方用 `atuin_free` 释放。

辅助函数集中在 `ffi.rs` 开头（与 starship-native 风格一致）：
`string_into_c`、`error_string`、`panic_to_error`，以及把 panic 转成错误字符串的
`ffi_guard_error!`。这样每个调用自带错误值，没有共享可变状态，也就没有
TLS/全局析构器在 `dlclose` 后悬挂的问题；`with_session!` 宏把“取锁 → 取活动
session → fork 检查”集中在一处，锁中毒通过 `into_inner()` 恢复。

### 2.6 search 结果中的 NUL

命令历史是用户输入，可能包含任意字节（NUL 除外，因为 shell argv 是 C string）。
拼接结果时用 `CString::new` 失败则截断到第一个 NUL，而不是让整个搜索失败。

### 2.6.1 统一的 `Session::search` 与 C ABI

`Session::search(SearchOptions)` 是唯一的搜索入口，内部按 `SearchMode` 分派，
对齐上游 `atuin search` 的 `Cmd::run`：

- `SearchMode::NonInteractive`：走上游 query engine，返回
  `SearchResult::Entries(Vec<History>)`；
- `SearchMode::Interactive`：打开上游完整 TUI，返回
  `SearchResult::Interactive(Option<String>)`（`None` = 用户取消）。

`search_mode`/`filter_mode`/`shell_up_key_binding`/`keymap_mode` 会像 CLI 一样
先覆盖到 settings 副本，再进入对应分支；limit/cwd/authors 等非交互字段在
interactive 模式下被忽略。

C FFI 侧仍通过 `atuin_search_options_t` 传入非交互选项；`atuin_search` 只接受
`SearchResult::Entries`。`atuin_search_prefix` 对应 `search_prefix` 快速路径
（等价于 `search-mode=prefix + author=$all-user + limit=N`），zsh 的
`atuin_search_prefix` builtin 与 autosuggest strategy 直接使用它，不再绕通用
`atuin_search` + 参数拼装。

`atuin_search_interactive` 增加了 `shell_up_key_binding` / `keymap_mode` 两个参数
（`ATUIN_KEYMAP_MODE_*`），按参数选择最具体的快速路径：

| 条件 | 调用的 Session 方法 | 对应官方用法 |
| --- | --- | --- |
| `keymap_mode != auto` | `search_interactive_with` | vi widget：`atuin search -i --keymap-mode=vim-normal/vim-insert` |
| 仅 `shell_up_key_binding` | `search_interactive_up` | UpArrow：`atuin search -i --shell-up-key-binding` |
| 都没有 | `search_interactive` | Ctrl+R：`atuin search -i` |
| （非交互）`atuin_search_prefix` | `search_prefix(query, limit)` | autosuggest：`--cmd-only --author '$all-user' --limit N --search-mode prefix` |

zsh 侧由 `ATUIN_SEARCH_SHELL_UP_KEY_BINDING` / `ATUIN_SEARCH_KEYMAP_MODE` 参数传入，
插件里 UpArrow widget 设前者、vi widget 设后者；pwsh 侧由
`Invoke-AtuinSearch -ExtraArgs` 解析 `--shell-up-key-binding` / `--keymap-mode=`。

### 2.6.2 会话统计（`atuin_stats`，对齐 starship_stats）

`Session` 内新增 `SessionStatsCounters`（`AtomicU64`）与 `created_at`，在
`history_start` / `history_end`(sync+async) / `search` / `search_prefix` /
`search_interactive_tui` 的关键点自增，`Session::stats()` 汇总为 `SessionStats`
快照（含 `in_flight_history_ends` 与 uptime）。FFI 导出 `atuin_stats(atuin_stats_t*)`
填充 C struct；zsh builtin `atuin_stats` 写 `ATUIN_STATS_*` 参数并（除非
`ATUIN_STATS_QUIET`）打印摘要，`ATUIN_STATS_VERBOSE` 追加 interactive 明细；
C# 侧 `AtuinSession.GetStats()` / `GetStatsReport()`，psm1 提供
`Get-AtuinNativeStats`。因为计数器属于 session，destroy→create 后从零开始。

### 2.7 进程内交互式 TUI（`atuin_search_interactive`）

TUI 本身**不是重新实现**：`Session::search` 的 interactive 分支直接调用上游
`command::client::search::interactive::history()`，因此官方 TUI 的
tabs/inspector/预览、keymap、filter/search mode 循环等全部同步继承。

进程内特有的两个改动：

- **输出/终端由上游 `Stdout` 管理**：stdout 非 TTY 时自动落到 `/dev/tty`
  / `CONOUT$`，raw mode 与 alternate screen 由上游 RAII 恢复；panic 仍被
  `ffi_guard!` 隔离。
- **Unix 事件源替换为 `in_process_event.rs`**：上游默认的
  `crossterm::event` 会懒加载注册 SIGWINCH signal-hook，注册后永不注销。
  独立二进制无所谓，但在 `dlclose` 场景会留下指向已卸载代码的信号处理器，
  窗口一变尺寸就段错误。`in_process_event.rs` 只提供 `poll/read` 两个函数，
  内部自管 `/dev/tty` + `poll(2)` + 字节级 CSI/UTF-8 解析，并把解析结果翻译成
  crossterm `Event` 后交给上游状态机；窗口尺寸变化靠上游循环周期重绘感知。
  解析范围与上游 TUI 启用的终端模式一致：
  - 按键（含 CSI/SS3 光标键、UTF-8）；
  - CSI-u / Kitty keyboard protocol (`ESC [ codepoint ; modifiers u`)：上游会
    push keyboard enhancement flags，支持该协议的终端（kitty/wezterm/foot 等）
    会用 CSI-u 上报所有按键，解析器将其归一化为与 legacy 字节相同的内部按键；
  - SGR (`ESC [ < ... M/m`) 与 X10 (`ESC [ M ...`) 鼠标报告。上游会开启
    any-event mouse tracking，因此**必须**消费鼠标移动事件；否则它们会被
    误判为 `Esc` 直接取消 TUI。滚轮事件映射为 `ScrollUp`/`ScrollDown`，
    与上游 `handle_mouse_input` 的选择行为对接；
  - 括号粘贴 (`ESC [ 200 ~ ... ESC [ 201 ~`)：上游把它作为 `Event::Paste`
    插入查询而不是逐键执行，因此解析器会跨多次 read 收集完整 payload；
  - 完整但无法识别的 CSI 序列映射为 `KeyCode::Null`（上游忽略），避免未知
    终端报告意外触发 Esc 取消。
- Windows 上 `crossterm::event` 只做 `WaitForMultipleObjects` /
  `ReadConsoleInputW`，无回调无线程，所以仍直接走 crossterm 事件源。

`enter_accept` 配置决定 Enter 是否返回 `__atuin_accept__:` 前缀；Esc /
Ctrl+C / Ctrl+G 返回取消，shell 保持原 buffer。

## 3. zsh 模块

### 3.1 头文件与链接

- 需要已 `./configure` 的 zsh 源码树生成 `Src/zsh.mdh` 与 `*.epro`
- 模块引用的 zsh 内部符号由宿主 `zsh` 二进制解析，因此链接参数为：
  - Linux: `-Wl,--allow-shlib-undefined -Wl,-z,lazy`
  - macOS: `-Wl,-undefined,dynamic_lookup`
- zsh 在所有平台硬编码模块后缀为 `.so`，macOS 上目标必须命名 `atuin_native.so`
- `INSTALL_RPATH`：Linux `$ORIGIN`，macOS `@loader_path`

### 3.2 metafy

Rust 返回的是 raw UTF-8。写入 zsh 参数前必须 metafy，否则非 ASCII 字节
会在 zsh 内部表示往返时损坏：

```c
static void set_str_param(const char *name, const char *val) {
    setsparam((char *)name, ztrdup_metafy(val ? val : ""));
}
```

### 3.3 零参数 builtin：用 zsh 变量交换输入输出

`module.c` 与 starship/zoxide 的 native 模块保持一致：builtin 不接收
argv，所有输入来自 zsh 参数，所有结果写回 zsh 参数。C 代码不再自行解析
`--search-mode`/`--sync`/limit 等参数，复杂 argv 解析由调用侧（zsh 脚本）
用变量表达：

```zsh
# history start
ATUIN_HISTORY_COMMAND="$1"
ATUIN_HISTORY_CWD="$PWD"
ATUIN_HISTORY_ID=""
atuin_history_start >/dev/null 2>&1
export ATUIN_HISTORY_ID="${ATUIN_HISTORY_ID:-}"

# history end（默认 fire-and-forget；调试时设 ATUIN_HISTORY_SYNC=1）
ATUIN_HISTORY_EXIT="$EXIT"
ATUIN_HISTORY_DURATION_NS="${duration:-0}"
ATUIN_HISTORY_SYNC=0
atuin_history_end
```

完整协议：

| Builtin | 输入参数 | 输出参数 |
| --- | --- | --- |
| `atuin_history_start` | `ATUIN_HISTORY_COMMAND`, `ATUIN_HISTORY_CWD`, `ATUIN_HISTORY_AUTHOR`, `ATUIN_HISTORY_AUTHOR_KIND`, `ATUIN_HISTORY_INTENT` | `ATUIN_HISTORY_ID` |
| `atuin_history_end` | `ATUIN_HISTORY_ID`, `ATUIN_HISTORY_EXIT`, `ATUIN_HISTORY_DURATION_NS`, `ATUIN_HISTORY_SYNC` | — |
| `atuin_search` | `ATUIN_SEARCH_QUERY`, `ATUIN_SEARCH_MODE`, `ATUIN_SEARCH_FILTER_MODE`, `ATUIN_SEARCH_CWD`, `ATUIN_SEARCH_EXCLUDE_CWD`, `ATUIN_SEARCH_EXITS`, `ATUIN_SEARCH_EXCLUDE_EXITS`, `ATUIN_SEARCH_BEFORE`, `ATUIN_SEARCH_AFTER`, `ATUIN_SEARCH_LIMIT`, `ATUIN_SEARCH_OFFSET`, `ATUIN_SEARCH_REVERSE`, `ATUIN_SEARCH_INCLUDE_DUPLICATES`, `ATUIN_SEARCH_AUTHORS`, `ATUIN_SEARCH_SHELLS` | `ATUIN_SEARCH_RESULT` |
| `atuin_search_interactive` | `ATUIN_SEARCH_QUERY` | `ATUIN_SEARCH_SELECTED` |

`ATUIN_SEARCH_MODE` / `ATUIN_SEARCH_FILTER_MODE` 使用与 CLI 相同的字符串
（`prefix`、`fuzzy`、`global`、`session` 等）；`ATUIN_SEARCH_AUTHORS` /
`ATUIN_SEARCH_SHELLS` / `ATUIN_SEARCH_EXITS` / `ATUIN_SEARCH_EXCLUDE_EXITS` 是
zsh 数组，分别对应上游可重复的 `--author` / `--shell` / `--exit` /
`--exclude-exit`。`ATUIN_HISTORY_AUTHOR_KIND` 取 `user`/`agent`
（大小写不敏感），对应 `atuin history start --author-kind`。这样官方 autosuggest 的
`--author '$all-user' --search-mode prefix` 在 zsh 侧就是：

```zsh
ATUIN_SEARCH_QUERY="$1"
ATUIN_SEARCH_LIMIT=1
ATUIN_SEARCH_MODE=prefix
ATUIN_SEARCH_AUTHORS=('$all-user')
atuin_search
```

### 3.4 C shim 保持极薄

由于不再解析参数，`module.c` 只需要读取 zsh 参数、构造 C ABI 结构、
调用 FFI、把返回字符串写回 zsh 参数。原来的 `atuin_builtin_util.{h,c}` 和
`test_zsh_module_unit.c` 已删除，CMake 不再构建独立的 C 参数解析单元测试。

### 3.5 preexec 计时必须在 start 之前打点

如果分成两个 hook（先 start、后打点），`add-zsh-hook` 按注册顺序执行，
duration 恒为 0。最终 `_atuin_native_preexec` 内部先写
`__atuin_preexec_time`，再调用 `atuin_history_start`。

### 3.6 hook 的返回状态

preexec 的最后一条命令如果是 `[[ cond ]] || return`，在 cond 为假时会把
失败状态带回 hook，`set -e` 的脚本会因此提前退出。测试和 hook 函数都要以
显式 `return 0` 收尾。

### 3.7 与官方 `atuin init zsh` / `atuin init powershell` 的差异

插件与官方脚本行为一致的部分：

- zsh-autosuggestions：**无条件**定义 `_zsh_autosuggest_strategy_atuin`（官方
  同名），并把 `"atuin"` 前插到 `ZSH_AUTOSUGGEST_STRATEGY`（变量未设置时直接
  设为 `("atuin")`）。因此插件在 zsh-autosuggestions 之前或之后 source 都生效；
  `_zsh_autosuggest_strategy_atuin_native` 仅作为旧配置的别名保留。旧实现只在
  `ZSH_AUTOSUGGEST_STRATEGY` 已存在时安装并只用 `atuin_native` 名字，属于
  与官方不一致的缺陷。
- OSC 133 使用官方同款 `__atuin_pty_proxy_owns_tty` 契约，`133;D` 格式与官方
  一致（`history_id=`，不再附带自定义的 `session_id=`）。
- 键位绑定尊重 `ATUIN_NOBIND`；pwsh 退出 TUI 后调用 `InvokePrompt` 并遵循
  `ATUIN_POWERSHELL_PROMPT_OFFSET`（未设置时按 prompt 行数推导）。

进程内无法提供、有意不做的官方功能（都需要外部 `atuin` 二进制 / daemon）：

- tmux popup 搜索（插件 `export ATUIN_TMUX_POPUP=false`，TUI 直接在当前终端绘制）；
- `atuin ai inline` 自然语言模式（`?` widget）；
- `atuin __internal prepare-search-index`（进程内 prefix search 直接查 SQLite）；
- PTY proxy 存活性探测以 `ATUIN_PTY_PROXY_ACTIVE` 近似：官方会再请求
  `atuin __internal pty-proxy-active` 确认 socket 存活，插件无法启动外部进程，
  但 proxy preamble 预设的 `__atuin_pty_proxy_owns_tty` 会被优先尊重。

### 3.7 非交互 shell 不注册 hook

在 `zsh -c` / 脚本中 source 插件时，preexec 会在每条 source 语句前触发
并携带空命令，污染测试历史。插件用 `[[ -o interactive ]]` 包住
`add-zsh-hook` 与 `zle -N` 注册。

### 3.8 交互式搜索 widget 不 fork

官方 widget 用 `output=$(_atuin_search_cmd)`，命令替换会 fork；进程内版本
直接调用 builtin：

```zsh
ATUIN_SEARCH_QUERY="$BUFFER"
atuin_search_interactive
```

builtin 把结果写入 `$ATUIN_SEARCH_SELECTED`（fork guard 只允许父进程），
widget 在返回后 `zle reset-prompt`，并按官方约定处理
`__atuin_accept__:` → `zle accept-line`。raw-mode TUI 退出后还要重新输出
`${zle_bracketed_paste[1]}`，与官方 widget 一致。

## 4. PowerShell 模块

### 4.1 LibraryImport 与自定义解析器

使用 .NET 7+ 源码生成器替代 `DllImport`。`LibraryImport` 需要 `partial` class，
字符串默认按 UTF-8 封送，AOT 友好。

`ATUIN_FFI_PATH` 用于显式指定 native 库；未设置时回退到默认解析
（程序集同目录）。CMake 会把 `libatuin_ffi.so` 复制到
`AtuinNative/bin/<Config>/net8.0`。

### 4.2 双环境块

Linux/macOS 上 PowerShell 的 `$env:` 和 .NET `Environment` 只更新 .NET
托管环境块，不调用 libc `setenv`。内嵌 Rust 通过 `std::env::var` 读取
`ATUIN_SHELL` / `ATUIN_SESSION` 会看不到。`AtuinEnvironment` 同时写两块：

```csharp
Environment.SetEnvironmentVariable(name, value);
if (!RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
    _ = SetEnv(name, value, 1);   // P/Invoke libc setenv
```

### 4.3 模块 scope 与 global 函数

`atuin-native.psm1` 定义 `function global:PSConsoleHostReadLine`。该函数在
global session state 执行，无法直接访问模块私有函数。因此模块导入时保存
模块对象：

```powershell
$script:AtuinNativeModule = $ExecutionContext.SessionState.Module
```

global wrapper 再通过 `& $script:AtuinNativeModule { ... }` 调回模块内部。

### 4.4 OnRemove 清理链

模块移除时必须恢复 PSReadLine 入口并释放 native session：

1. 恢复/删除 `PSConsoleHostReadLine`
2. 清空 `ATUIN_SESSION` / `ATUIN_PID`
3. `Dispose()` native session → tokio `shutdown_timeout` → 释放 SQLite/key

集成测试会执行 3 次 import/remove 循环验证该路径。

### 4.5 进程内 TUI 的 PSReadLine 交互

模块导入时自动执行 `Enable-AtuinSearchKeys -CtrlR $true -UpArrow $true`
（等价于官方 init 脚本的最后一行），所以用户 source / Import-Module 后
Ctrl+R / UpArrow 立即可用，无需手动 bindkey。zsh 侧同样在交互 shell 中
安装官方 `atuin init zsh` 的整组默认 bindkey。

`Enable-AtuinSearchKeys` 的 Ctrl+R ScriptBlock 直接调用
`AtuinSession.SearchInteractive`。PSReadLine 在 handler 返回前不会读取输入，
因此 FFI 可以安全地在同一线程接管终端；返回后
`Set-AtuinCommandLine` / `AcceptLine` 恢复行编辑。pty 集成测试验证了这条
路径：Ctrl+R → TUI 打开 → Enter → 选中命令被 `AcceptLine` 执行。

pwsh 7.6 启动时会发送 ESC[6n 光标位置查询；pty 不是终端模拟器，必须由
测试应答，否则 PSReadLine 卡在启动阶段。

## 5. 构建系统

### 5.1 Cargo 增量构建

`add_custom_target(atuin_ffi ALL)` 每次构建都会跑 `cargo build`（cargo 自己秒退），
真正的 OUTPUT rule 挂在 `libatuin_ffi.so` 上，Ninja 只在输出缺失或依赖变化时执行：

```cmake
add_custom_command(
    OUTPUT "${FFI_SHARED}"
    COMMAND ${CARGO} build ${CARGO_RELEASE_FLAG}
    ...
)
```

### 5.2 Multi-Config

`CARGO_PROFILE_DIR`、`CARGO_RELEASE_FLAG`、`PWSH_OUT_DIR` 都通过
`$<IF:$<CONFIG:Release,...>` / `$<CONFIG>` 适配 Ninja Multi-Config 与 Visual Studio。
zsh 模块的 `LIBRARY_OUTPUT_DIRECTORY` 在 multi-config 下会自动追加配置子目录，
因此测试目标用 `zsh_src/build/$<CONFIG>`。

### 5.3 CMake 内联脚本的转义

zsh 测试逻辑不写进 CMake `COMMAND` 内联字符串。`${var}`、引号层级和
`$()` 在 CMake 与 zsh 两层展开下极易出错；统一提取为 `tests/*.sh`。

## 6. 测试策略

| 层级 | 目标 | 工具 |
| --- | --- | --- |
| Rust 单元测试 | ffi.rs 全部导出函数 + TUI 状态机/编辑逻辑 | `cargo test` |
| C 系统测试 | dlopen 调用所有 FFI 导出，fork guard | `ffi_smoke` |
| zsh 集成测试 | 模块加载、history 往返、search、unload | `test_zsh.sh` |
| zsh 系统测试 | `$()`/`&`/管道/子 shell/进程替换后父进程仍可用 | `test_fork_zsh.sh` |
| zsh 生命周期测试 | 5 次 load/unload，检查线程数 | `test_unload_zsh.sh` |
| zsh 插件集成测试 | source 真实插件并驱动 hook | `test_plugin_zsh.sh` |
| zsh TUI pty 测试 | 真实按键驱动全屏 TUI、widget、取消与终端恢复 | `test_tui_zsh.py` |
| pwsh 单元测试 | Session/Environment 托管封装 | 无依赖 console runner |
| pwsh 集成测试 | Add-Type、manifest import、PSReadLine 恢复 | `test_pwsh.ps1` |
| pwsh TUI pty 测试 | SearchInteractive 选中/取消 + Ctrl+R handler | `test_tui_pwsh.py` |

运行方式见 `docs/testing.md`。

## 7. 跨平台移植注意

### Linux → macOS

- FFI 库后缀为 `.dylib`，zsh 模块仍为 `.so`
- 需要 `install_name_tool -id "@rpath/libatuin_ffi.dylib"` 重写 LC_ID_DYLIB
- zsh 可能使用长符号名（`boot_atuin_native` 而非 `boot_`）

### Linux → Windows

- zsh 模块不构建
- native 库为 `atuin_ffi.dll`，需要 MSVC Rust target
- PowerShell manifest 使用 `NestedModules` 加载 `AtuinNative.dll`
- `AtuinEnvironment` 只更新 .NET 环境块（Windows 无 libc setenv 问题）
