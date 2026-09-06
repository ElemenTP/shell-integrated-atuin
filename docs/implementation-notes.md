# 实现笔记

本文档记录把 Atuin 改造为 shell 进程内插件过程中的关键实现细节与踩坑记录。

## 1. Atuin 内部改造

### 1.1 常驻 Session 使用 multi-thread tokio runtime

`atuin-client/src/session.rs` 持有一个多线程 `Runtime`：

```rust
let runtime = tokio::runtime::Builder::new_multi_thread()
    .worker_threads(2)
    .enable_all()
    .build()?;
```

选择 multi-thread 的原因：shell precmd 中的 `atuin_history_end` 必须等价于
官方的 `(atuin history end ... &)`。如果使用 current-thread runtime，
`runtime.spawn()` 只是把任务放进本地队列；builtin 返回 zsh 后没有
`block_on` 继续驱动 runtime，任务要等下一次 FFI 调用才真正执行——不是真正的
fire-and-forget。多线程 runtime 的 worker 会在 builtin 返回后立即消费任务。

对“worker 线程污染 zsh 主线程 / 难以卸载”的应对：

1. **不保留全局 TLS**：FFI crate 不使用 `thread_local!`；错误槽是全局 `Mutex`
2. **fork guard**：`$()`、`&`、管道等 fork 子进程在触碰 runtime 前被拒绝
3. **显式 shutdown**：`Session::drop` 调用 `shutdown_timeout(10s)`，
   等待 worker 退出和在途 history_end 完成后才允许 `dlclose`
4. **线程数受控**：`worker_threads(2)`，一个 shell 只常驻少量 worker

`shutdown_timeout` 的调用链：

```text
zmodload -u / Remove-Module
  → cleanup_ / OnRemove
  → atuin_session_destroy
  → Session::drop
  → Runtime::shutdown_timeout(10s)     # 等待在途任务 + tokio worker
  → history.db pool close              # 等待 sqlx worker
  → records.db pool close              # 等待 sqlx worker
  → Settings::close_meta_store()       # 等待 meta.db 的 sqlx worker
  → dlclose
```

sqlx SQLite 连接不是 tokio task，而是自己的 `sqlx-sqlite-worker-*` 线程。
仅关闭 tokio runtime 后 zsh 仍会残留 3 个 sqlx 线程（history / records /
meta 三个 pool）。`Session::shutdown` 必须逐个 `pool.close().await`。
meta store 由 `Settings` 的进程级 `OnceCell` 持有，无法原地重建；为支持
`zmodload -u` 后重新 `zmodload`，其全局缓存改为可替换的
`OnceLock<Mutex<Option<Arc<MetaStore>>>>`，`close_meta_store()` 取出旧 store
关闭，下一次 `meta_store()` 再建新 pool。

### 1.2 Settings::new 与 meta store 的隐式依赖

`Session::new` 最初使用：

```rust
Settings::builder()?.build()?.try_deserialize()?
```

这**不会**注册 `Settings::host_id()` 需要的全局 meta store 配置。
在 `history_end` 需要把加密 record 写入 records.db 时，第一次调用 `Settings::host_id()`
会报 `meta store config not set — Settings::new() has not been called`。

正确写法是直接调用 `Settings::new()?`，它内部执行同样的配置解析并注册 meta store。

`Session::new` 的 data-dir 参数是 `Option<&Path>`：`Some` 使用调用方显式传入的目录
（`ATUIN_DATA_DIR`），`None` 使用 `settings.db_path` / `record_store_path` /
`key_path`，这样 config.toml 中的 `data_dir` 也能生效。

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
- `#[cfg(feature = "in-process")] pub mod session;` 只新增模块
- `HistoryCaptured` 新增字段使用 builder 的 `default`，不影响既有调用

## 2. Rust FFI 层

### 2.1 Rust 2024 的 unsafe attribute

Rust 2024 edition 中 `#[no_mangle]` 是 unsafe attribute：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn atuin_session_create(...) -> ... { ... }
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

实现方式是在 `SessionHandle` 记录创建时的 PID：

```rust
pub struct SessionHandle {
    session: Session,
    creator_pid: u32,
    uuid: CString,
}
```

所有会触碰 tokio runtime 的入口先比较 `std::process::id()`。
`atuin_session_destroy` 同样带 guard：fork 子进程不会 drop 父进程的 Box（泄漏由 `_exit` 回收）。

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
每次查询泄漏一个 CString。现在 UUID 在 session 创建时缓存到 `SessionHandle.uuid`：

```rust
const char *atuin_session_uuid(atuin_session_t *s) {
    ...
    return h.uuid.as_ptr();
}
```

返回指针由 session 持有，文档明确“不可释放”。

### 2.5 错误槽使用全局 Mutex 而不是 TLS

原因与 starship-native 相同：TLS 析构器会注册在宿主线程，
macOS/Windows 在 `dlclose` 后可能调用悬挂析构器。全局 `Mutex<Option<CString>>`
没有每线程状态，FFI 调用又被 shell 单线程串行化，无实际争用。

### 2.6 search 结果中的 NUL

命令历史是用户输入，可能包含任意字节（NUL 除外，因为 shell argv 是 C string）。
拼接结果时用 `CString::new` 失败则截断到第一个 NUL，而不是让整个搜索失败。

### 2.7 进程内交互式 TUI（`atuin_search_interactive`）

官方 `atuin search -i` 是一个独立进程，退出即回收。进程内实现必须自己
保证终端状态与卸载安全：

- 输出写 stdout（TTY 时）或 `/dev/tty` / `CONOUT$`（stdout 被重定向时）
- zsh/pwsh 的编辑器本身已把终端置于自己的模式，TUI 进入前保存 termios，
  Drop 时逐字节恢复，panic 也被 `ffi_guard!` 包住后先恢复再返回
- 交替屏幕（`?1049h/l`）由 RAII `ActiveScreen` 保证退出；即使搜索
  refresh 出错也先离开交替屏幕、再恢复 raw mode
- **Unix 上不使用 `crossterm::event`**：它会懒加载注册 SIGWINCH
  signal-hook，注册后永不注销。独立二进制无所谓，但在 `dlclose` 场景
  会留下指向已卸载代码的信号处理器，窗口一变尺寸就段错误。`tui_input.rs`
  改为自己 `open("/dev/tty")` + `poll(2)` + 字节级 CSI/UTF-8 解析，
  尺寸变化靠 100ms 的 poll 超时重查 `TIOCGWINSZ` 感知
- Windows 上 `crossterm::event` 只做 `WaitForMultipleObjects` /
  `ReadConsoleInputW`，无回调无线程，可以安全保留
- 注意 ratatui 的 `Terminal::clear()` 会通过 ESC[6n 查询光标位置并间接
  初始化 crossterm event 模块，因此 TUI 不调用它；首帧 draw 天然全量绘制

TUI 查询不通过子进程：`Session::search(mode, query, limit)` 在当前
history.db 连接池上实时查询（Fuzzy 默认，Ctrl+S 循环 Prefix/FullText），
上限 200 条。`enter_accept` 配置决定 Enter 是否返回 `__atuin_accept__:`
前缀；Esc / Ctrl+C / Ctrl+G 返回取消，shell 保持原 buffer。

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
static void set_str_param(const char *name, char *val) {
    if (!val) return;
    setsparam((char *)name, metafy((char *)val, strlen(val), META_DUP));
}
```

### 3.3 参数协议避免命令替换

官方 hook 用 `id=$(atuin history start ...)`。在本方案中这个写法必然触发 fork guard。
因此 `atuin_history_start` 同时写入 `$ATUIN_HISTORY_ID`，插件只重定向 stdout：

```zsh
ATUIN_HISTORY_ID=""
atuin_history_start "$1" "$PWD" >/dev/null 2>&1
export ATUIN_HISTORY_ID="${ATUIN_HISTORY_ID:-}"
```

`atuin_search` 同样写入 `$ATUIN_SEARCH_RESULT`，供 autosuggest strategy 使用。

### 3.4 纯参数解析 helper 独立成文件

`atuin_module.c` 依赖 zsh 内部头文件，无法被普通 C 编译器链接。
因此 duration/limit/exit 解析放在 `atuin_builtin_util.{h,c}`，
模块与 `tests/test_zsh_module_unit.c` 共同编译它。

### 3.5 preexec 计时必须在 start 之前打点

如果分成两个 hook（先 start、后打点），`add-zsh-hook` 按注册顺序执行，
duration 恒为 0。最终 `_atuin_native_preexec` 内部先写
`__atuin_preexec_time`，再调用 `atuin_history_start`。

### 3.6 hook 的返回状态

preexec 的最后一条命令如果是 `[[ cond ]] || return`，在 cond 为假时会把
失败状态带回 hook，`set -e` 的脚本会因此提前退出。测试和 hook 函数都要以
显式 `return 0` 收尾。

### 3.7 非交互 shell 不注册 hook

在 `zsh -c` / 脚本中 source 插件时，preexec 会在每条 source 语句前触发
并携带空命令，污染测试历史。插件用 `[[ -o interactive ]]` 包住
`add-zsh-hook` 与 `zle -N` 注册。

### 3.8 交互式搜索 widget 不 fork

官方 widget 用 `output=$(_atuin_search_cmd)`，命令替换会 fork；进程内版本
直接调用 builtin：

```zsh
atuin_search_interactive "$BUFFER"
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
| C 单元测试 | zsh builtin 参数解析 helper | 普通 cc 可执行文件 |
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
