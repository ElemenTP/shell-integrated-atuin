# Atuin Native Shell Plugin

将 [Atuin](https://atuin.sh) shell 历史记录编译为 Shell 原生动态库，
在 zsh / PowerShell 进程内完成历史记录与 prefix search，消除每条命令
多次启动 `atuin` 进程的开销，并复用 SQLite 连接、Settings 和加密 key。

## 工作原理

```
传统模式（每命令 2+ 次 fork + exec）:
  preexec:   ATUIN_HISTORY_ID=$(atuin history start --hook -- "$1")   # 1 次进程
  precmd:    (atuin history end --hook ... &)                          # 1 次进程
  自动补全:   atuin search --cmd-only --limit 1 ...                    # 每次按键

Native 模式（进程内，0 次 fork）:
  preexec:   atuin_history_start "$1" "$PWD"   # builtin → FFI → Session
  precmd:    atuin_history_end "$ID" "$EXIT" "$duration"   # builtin → FFI
  自动补全:   atuin_search "$1" 1               # builtin，结果写入 $ATUIN_SEARCH_RESULT
  交互式TUI:  atuin_search_interactive "$BUFFER"  # builtin，全屏 TUI，Ctrl+R widget
```

`Session` 在 shell 进程内常驻，持有 `history.db` / `records.db` 连接、
tokio multi-thread runtime 与加密 key；history/end/search/TUI 都复用上游
`atuin` 命令实现而不是重新发明。precmd 中的 `history_end` 以
fire-and-forget 方式提交到 worker 线程，builtin 立即返回；卸载模块时
先等待 tokio worker 退出，再关闭 history/records/meta 三个 SQLite pool，
最后才允许 `dlclose`。

## 支持平台

| Shell | 平台 | 加载方式 |
|-------|------|----------|
| zsh | Linux, macOS | `zmodload atuin_native` |
| pwsh (PowerShell 7+) | Windows, Linux, macOS | `Import-Module atuin-native` |

当前原生路径覆盖 shell hook、prefix search、autosuggest 与交互式全屏 TUI
搜索。TUI 直接复用上游 `atuin search --interactive` 的完整 ratatui 状态机
（tabs/inspector/预览/键位配置等），仅 Unix 输入层做了 `dlclose` 安全替换。
daemon 模式与网络 sync 仍使用官方 `atuin` 二进制。

## 编译

### 前置条件

- CMake 3.21+
- Rust 1.95+（`cargo`）
- GCC / Clang（zsh shim）
- .NET SDK 8.0+（仅 pwsh 模块）
- 已 `configure` 的 zsh 源码树（仅 zsh 模块，CMake 也可自动下载）

### 一键构建（自动下载依赖）

```bash
cmake -B build -S . -G "Ninja Multi-Config"
cmake --build build --config Release
```

### 使用本地源码

```bash
cmake -B build -S . -G "Ninja Multi-Config" \
  -DATUIN_SOURCE=/path/to/atuin-in-process \
  -DZSH_SOURCE=/path/to/zsh-5.9.2
cmake --build build --config Release
```

`ATUIN_SOURCE` 必须包含带 `in-process` feature 的 Atuin 源码树。
`ZSH_SOURCE` 必须已执行过 `./configure` 和 `make -C Src -f Makemod zsh.mdh`
（或留空 `AUTO` 让 CMake 自动下载生成）。

### 仅构建特定模块

```bash
# 仅 zsh
cmake -B build ... -DBUILD_PWSH_MODULE=OFF

# 仅 pwsh
cmake -B build ... -DBUILD_ZSH_MODULE=OFF
```

### 安装

```bash
# zsh: 安装到 ~/.local/lib/zsh/atuin-native
# pwsh: 安装到 ~/.local/share/pwsh/modules/atuin-native
cmake --install build --config Release --prefix ~/.local

# 系统级安装
sudo cmake --install build --config Release --prefix /usr/local
```

### CMake 选项

| 选项 | 默认值 | 说明 |
|------|--------|------|
| `ATUIN_SOURCE` | `AUTO` | Atuin 源码：`AUTO` 从 GitHub clone `in-process` 分支，或本地路径 |
| `ZSH_SOURCE` | `AUTO` | zsh 源码：`AUTO` 下载 zsh 5.9.2，或已配置的源码树路径 |
| `ZSH_VERSION` | `5.9.2` | 下载 zsh 的版本（`ZSH_SOURCE=AUTO` 时） |
| `BUILD_ZSH_MODULE` | `ON`（Windows 为 `OFF`） | 构建 zsh 可加载模块 |
| `BUILD_PWSH_MODULE` | `ON` | 构建 PowerShell 二进制模块 |
| `BUILD_TESTS` | `ON` | 生成测试目标 |

## 使用

### zsh

仓库根目录的 `atuin-native.plugin.zsh` 符合标准插件入口约定，可被主流
插件管理器识别。

#### 方式一：插件管理器（推荐）

先编译并安装，然后在插件管理器里加载本仓库：

| 管理器 | 配置示例 |
| --- | --- |
| oh-my-zsh | clone 到 `~/.oh-my-zsh/custom/plugins/atuin-native`，`plugins=(... atuin-native)` |
| zinit | `zinit light <user>/shell-integrated-atuin` |
| antigen | `antigen bundle <user>/shell-integrated-atuin` |
| zplug | `zplug "<user>/shell-integrated-atuin", use:"atuin-native.plugin.zsh"` |
| zgen | `zgen load <user>/shell-integrated-atuin` |

插件按以下顺序查找编译产物：

1. `$ATUIN_NATIVE_DIR`（显式指定）
2. 插件脚本自身目录（`cmake --install` 布局）
3. 常见安装前缀（`~/.local/lib/zsh/atuin-native`、`/usr/local/lib/zsh/atuin-native` 等）

#### 方式二：直接 source

```zsh
# .zshrc
export ATUIN_NATIVE_DIR="$HOME/.local/lib/zsh/atuin-native"
source "$ATUIN_NATIVE_DIR/atuin-native.plugin.zsh"
```

插件自动完成：

1. `zmodload atuin_native`
2. 初始化 `ATUIN_SESSION` / `ATUIN_SHLVL`
3. 注册 preexec / precmd / zshaddhistory hook
4. 安装 zsh-autosuggestions 的 `atuin_native` strategy
5. 定义 `atuin-search` / `atuin-up-search` 等兼容 ZLE widget
6. 按 `atuin init zsh` 的默认键位自动绑定 Ctrl+R / UpArrow（emacs/viins/vicmd）
7. 支持 OSC 133 标记（`ATUIN_PTY_PROXY_ACTIVE` 时）

#### Builtin 命令

| 命令 | 说明 |
|------|------|
| `atuin_history_start "cmd" [cwd]` | 记录命令开始，写入 `$ATUIN_HISTORY_ID` 并输出 ID |
| `atuin_history_end <id> <exit> [duration_ns] [--sync]` | 记录命令结束。默认 fire-and-forget，`--sync` 同步等待并报告错误 |
| `atuin_search <query> [limit]` | 前缀搜索，结果写入 `$ATUIN_SEARCH_RESULT` |
| `atuin_search_interactive [query]` | 全屏交互式 TUI 搜索（上游完整 TUI），结果写入 `$ATUIN_SEARCH_SELECTED`；返回 0=选中、1=取消、2=错误 |
| `atuin_session_id` | 输出并写入 `$ATUIN_SESSION` |
| `atuin_version` | 输出并写入 `$ATUIN_NATIVE_VERSION` |

**fork 安全**：在 `$(...)`、`&`、管道非末位、子 shell 中调用 builtin 时，
FFI fork guard 会拒绝调用并返回非零，不会崩溃。hook 脚本因此全部使用参数传递，
不使用命令替换。

**交互式 TUI 按键**：输入即模糊过滤（默认 fuzzy），`↑/↓`（或 `Ctrl+P/N`）
选择，`Enter` 选中（`enter_accept = true` 时直接执行），`Tab` 只替换不执行，
`Esc` / `Ctrl+C` / `Ctrl+G` 取消，`Ctrl+S` 在 fuzzy → prefix → full-text 间
循环，`Ctrl+U` 清空、`Ctrl+W` 删词、`Ctrl+L` 重绘。

### pwsh

```powershell
Import-Module atuin-native       # 安装后
# 或
Import-Module /path/to/pwsh_src/AtuinNative/bin/Release/net8.0/atuin-native.psd1

Get-AtuinNativeVersion
Enable-AtuinSearchKeys           # 可选：重新绑定（导入时已自动绑定）
```

模块导入后自动替换 `PSConsoleHostReadLine`，在每行命令前后调用进程内
`HistoryStart` / `HistoryEnd`，并**自动**把 Ctrl+R / UpArrow 绑定到进程内
全屏 TUI 搜索（与 `atuin init powershell` 一致）；移除模块时恢复原始
函数并释放 native session。

直接使用托管封装：

```powershell
$session = [AtuinNative.AtuinSession]::new($env:ATUIN_DATA_DIR)
$id = $session.HistoryStart("my-command", (Get-Location).Path)
$session.HistoryEnd($id, 0, 0, $true)   # sync
$results = $session.SearchPrefix("my-command", 5)
$selected = $session.SearchInteractive("my")   # 全屏 TUI；取消返回 $null
$session.Dispose()
```

如果 FFI 库不在 `AtuinNative.dll` 同目录，设置 `$env:ATUIN_FFI_PATH`。

## 环境变量

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `ATUIN_DATA_DIR` | `~/.local/share/atuin` | 数据目录（`history.db`、`records.db`、key） |
| `ATUIN_SESSION` | 插件自动设置 | 当前 shell 会话 UUID |
| `ATUIN_NATIVE_DIR` | 插件自动探测 | zsh 模块与 FFI 库目录 |
| `ATUIN_FFI_PATH` | `AtuinNative.dll` 同目录 | pwsh 使用的 FFI 库路径 |

## 测试

```bash
cmake -B build -S . \
  -DATUIN_SOURCE=/path/to/atuin \
  -DZSH_SOURCE=/path/to/zsh-5.9.2

# 全部测试
cmake --build build --config Release --target check-all

# 单独目标
cmake --build build --config Release --target test-rust
cmake --build build --config Release --target test-zsh
cmake --build build --config Release --target test-fork-zsh
cmake --build build --config Release --target test-unload-zsh
cmake --build build --config Release --target test-plugin-zsh
cmake --build build --config Release --target test-tui-zsh
cmake --build build --config Release --target test-install-zsh
cmake --build build --config Release --target test-pwsh-unit
cmake --build build --config Release --target test-pwsh
cmake --build build --config Release --target test-tui-pwsh
cmake --build build --config Release --target test-install-pwsh
```

测试分层与运行细节见 [`docs/testing.md`](docs/testing.md)。

## 文档

- [docs/architecture.md](docs/architecture.md) — 架构设计与核心决策
- [docs/implementation-notes.md](docs/implementation-notes.md) — 实现细节与踩坑记录
- [docs/testing.md](docs/testing.md) — 测试矩阵与运行方法

## 目录结构

```
shell-integrated-atuin/
├── rust_src/                     # Rust FFI crate (cdylib, 薄 FFI 边界)
├── zsh_src/                      # zsh 模块 + 插件
├── pwsh_src/                     # PowerShell 二进制模块 + 单元测试项目
├── tests/                        # 系统测试 / 集成测试 / C 单元测试
├── docs/                         # 架构与实现文档
├── atuin-native.plugin.zsh       # 插件管理器入口
├── atuin -> ../atuin/            # Atuin 源码（带 in-process feature）
└── CMakeLists.txt
```

## License

MIT — 与 Atuin 相同。
