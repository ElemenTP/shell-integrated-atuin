# 测试指南

## 测试矩阵

| 测试 | 类型 | 入口 | 验证内容 |
| --- | --- | --- | --- |
| `cargo test` | Rust 单元测试 | `rust_src/src/ffi.rs` | FFI 生命周期、单 session 契约（重复 init 幂等、无 session 报错、shutdown 幂等、重建）、NULL 安全、返回值错误协议、history 往返、负 duration 拒绝、`author_kind`（`$all-agent`/`$all-user` 过滤）、可重复 exit 过滤（include/exclude）、search limit、UUID 稳定性、record store、同 session 并发调用、会话统计计数与重建归零 |
| `ffi_smoke` | C 系统测试 | `tests/ffi_smoke.c` | `dlopen` 加载真实 `libatuin_ffi`，遍历全部导出函数、错误返回协议、fork guard、可重复 exit 过滤、`author_kind` 转发与校验、`atuin_stats` 计数与重建归零 |
| `test-zsh` | zsh 集成测试 | `tests/test_zsh.sh` | `zmodload`、builtin 变量参数协议、history 往返、search / search_prefix、`ATUIN_HISTORY_AUTHOR_KIND`、`ATUIN_SEARCH_EXITS`/`ATUIN_SEARCH_EXCLUDE_EXITS`（数组与标量两种写法）、`atuin_stats`（`-q` argv 抑制摘要、参数非法路径）、unload |
| `test-fork-zsh` | zsh 系统测试 | `tests/test_fork_zsh.sh` | `$()`、`&`、管道、子 shell、进程替换、嵌套替换都必须被 fork guard 拒绝（断言 stderr 的 `refusing call in forked child process`），fork 后父进程可用 |
| `test-unload-zsh` | zsh 生命周期测试 | `tests/test_unload_zsh.sh` | 5 次 load/unload 不崩溃、不泄漏 tokio worker 线程 |
| `test-plugin-zsh` | zsh 插件集成测试 | `tests/test_plugin_zsh.sh` | source 真实 `*.plugin.zsh`，驱动 preexec/precmd/zshaddhistory/autosuggest（`atuin_native` strategy、已加载/未加载 zsh-autosuggestions 两种顺序，均走 `atuin_search_prefix`） |
| `test-tui-zsh` | zsh TUI pty 集成测试 | `tests/test_tui_zsh.py` | 真实 pty 下打开上游完整 TUI、Enter/↑/Esc、鼠标移动/滚轮、括号粘贴、`$ATUIN_SEARCH_SELECTED`、默认 Ctrl+R / UpArrow bindkey、终端恢复 |
| `test-install-zsh` | zsh 安装布局测试 | `tests/test_tui_zsh.py`（`PLUGIN` 指向安装 prefix） | `cmake --install` 后 source 安装出来的插件，Ctrl+R / UpArrow 仍能触发上游完整 TUI |
| `test-pwsh-unit` | PowerShell 托管单元测试 | `pwsh_src/AtuinNative.Tests` | `AtuinSession` / `AtuinEnvironment` 的断言（单 session Init/Shutdown 契约、`author_kind`、可重复 exit 过滤、负 duration/limit/offset 校验、被过滤命令的 `HistoryStart` 返回 null、stats 计数与重建归零） |
| `test-pwsh` | pwsh 集成测试 | `tests/test_pwsh.ps1` | Add-Type、manifest import、history/search、`Get-AtuinNativeStats`、PSConsoleHostReadLine 恢复、3 次模块循环 |
| `test-tui-pwsh` | pwsh TUI pty 集成测试 | `tests/test_tui_pwsh.py` | 全新 shell（`$LASTEXITCODE` 未设置 + `$ErrorActionPreference='Stop'`）下 `PSConsoleHostReadLine` 仍记录历史、`SearchInteractive` 选中/取消、导入时自动绑定的 Ctrl+R / UpArrow 打开 TUI 并 `AcceptLine`、退出后 `InvokePrompt` 复位提示符 |
| `test-install-pwsh` | pwsh 安装布局测试 | `tests/test_tui_pwsh.py`（`DLL_DIR` 指向安装 prefix） | `cmake --install` 后的模块导入自动绑定 Ctrl+R / UpArrow |

## 前置条件

```bash
# 系统工具
zsh 5.9+            # zsh 集成测试
pwsh 7.2+           # PowerShell 测试
cmake 3.21+ ninja
cargo / rustc       # Rust FFI
dotnet SDK 8+       # pwsh 模块与托管单元测试

# 本地依赖源码（离线环境）
/path/to/atuin-with-in-process-feature
/path/to/zsh-5.9.2-configured   # Src/zsh.mdh 已生成
```

zsh 源码的头文件生成方法：

```bash
cd /path/to/zsh-5.9.2
./configure --disable-gdbm --disable-pcre
top_srcdir="$PWD" bash Src/mkmakemod.sh Src Makemod
make -C Src -f Makemod zsh.mdh
```

## 一键运行

```bash
cmake -B build -S . \
  -DATUIN_SOURCE=/path/to/atuin \
  -DZSH_SOURCE=/path/to/zsh-5.9.2
cmake --build build --config Release --target check-all
```

`check-all` 会依次执行：

1. Rust FFI 单元测试
2. C FFI 冒烟/系统测试（CTest）
3. zsh 集成/系统/生命周期/插件测试
4. zsh 与 pwsh 的全屏 TUI pty 集成测试
5. `cmake --install` 安装布局下的 zsh / pwsh TUI 回归测试
6. pwsh 托管单元测试
7. pwsh 集成测试

只看 CTest 结果：

```bash
ctest --test-dir build -C Release --output-on-failure
```

## 单独运行

```bash
# Rust
cmake --build build --config Release --target test-rust
# 或
cd rust_src && cargo test

# zsh
cmake --build build --config Release --target test-zsh
cmake --build build --config Release --target test-fork-zsh
cmake --build build --config Release --target test-unload-zsh
cmake --build build --config Release --target test-plugin-zsh
cmake --build build --config Release --target test-tui-zsh
cmake --build build --config Release --target test-install-zsh

# pwsh
cmake --build build --config Release --target test-pwsh-unit
cmake --build build --config Release --target test-pwsh
cmake --build build --config Release --target test-tui-pwsh
cmake --build build --config Release --target test-install-pwsh

# C 测试直接运行
./build/Release/ffi_smoke rust_src/target/release/libatuin_ffi.so
```

也可以直接调用测试脚本：

```bash
MODULE_DIR=$PWD/zsh_src/build/Release zsh tests/test_zsh.sh
MODULE_DIR=$PWD/zsh_src/build/Release zsh tests/test_fork_zsh.sh
MODULE_DIR=$PWD/zsh_src/build/Release zsh tests/test_unload_zsh.sh
MODULE_DIR=$PWD/zsh_src/build/Release zsh tests/test_plugin_zsh.sh
MODULE_DIR=$PWD/zsh_src/build/Release REPO_ROOT=$PWD python3 tests/test_tui_zsh.py

DLL_DIR=$PWD/pwsh_src/AtuinNative/bin/Release/net8.0 \
  pwsh -NoProfile -File tests/test_pwsh.ps1

DLL_DIR=$PWD/pwsh_src/AtuinNative/bin/Release/net8.0 python3 tests/test_tui_pwsh.py

ATUIN_FFI_PATH=$PWD/rust_src/target/release/libatuin_ffi.so \
DOTNET_ROLL_FORWARD=Major \
  dotnet run --project pwsh_src/AtuinNative.Tests -c Release
```

## 测试环境变量

| 变量 | 默认值 | 作用 |
| --- | --- | --- |
| `MODULE_DIR` | `$PWD/zsh_src/build` | zsh 测试的模块目录 |
| `REPO_ROOT` | `$PWD` | 插件集成测试的仓库根目录 |
| `ATUIN_DATA_DIR` | `/tmp/atuin-*-$$` | 测试数据库隔离目录（各测试自动设置；FFI 不再接收该参数，由 Settings 解析） |
| `ATUIN_CONFIG_DIR` | `<ATUIN_DATA_DIR>/config` | 测试专用 config.toml 目录，避免读写真实的 `~/.config/atuin` |
| `CYCLES` | `5` | unload 测试循环次数 |
| `PWSH` | `pwsh` | TUI pty 测试使用的 PowerShell 可执行文件 |
| `DLL_DIR` | pwsh Release 输出目录 | pwsh 测试模块目录 |
| `ATUIN_FFI_PATH` | 模块/程序集同目录 | 显式指定 native FFI 库路径 |

## fork guard 测试注意事项

被 guard 拒绝的 builtin 在 fork 子进程中返回非零状态，并在 stderr 输出
`refusing call in forked child process`。`test_fork_zsh.sh` 用统一的
`expect_fork_rejected` 断言这一点：把子进程的 stderr 重定向到文件，再检查
消息（进程替换等异步场景会重试等待）。

在 `set -e` 脚本中，预期失败的调用必须放在 `if` / `while` 条件中，或加
`|| true`，否则测试脚本会被非零状态中止——这不是模块崩溃：

```zsh
errfile=$(mktemp)
: > "$errfile"
# 预期非零：或放在 if 条件里，或显式 || true
sub_id=$(atuin_history_start 2>"$errfile") || true
grep -q "forked child process" "$errfile"
```

注意管道的退出状态是最后一段（`cat`）的状态；`test_fork_zsh.sh` 依赖
`set -o pipefail`，否则非末位的 builtin 失败会被 `cat` 的成功掩盖。

## 线程泄漏检查

`test_unload_zsh.sh` 在 Linux 上通过 `/proc/$$/task` 比较 load/unload
前后的线程数：加载并触发一次异步 `history_end` 后，tokio + sqlx worker
必须出现；`shutdown_timeout` + 显式 SQLite pool close 后必须全部退出。
允许 ±2 的 zsh 自身波动。macOS 会自动跳过该检查。

## CI 建议

最小矩阵：

- **Linux**：`cmake --build build --config Release --target check-all`
- **macOS**：`check-all`（`BUILD_ZSH_MODULE=ON`）
- **Windows**：`-DBUILD_ZSH_MODULE=OFF -DBUILD_PWSH_MODULE=ON`，运行
  `test-pwsh-unit` 与 `test-pwsh`

离线 CI 务必显式传 `-DATUIN_SOURCE=... -DZSH_SOURCE=...`，避免 FetchContent 访问外网。
