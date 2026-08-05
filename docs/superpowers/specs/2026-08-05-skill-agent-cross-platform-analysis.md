# skill_agent（Harness）跨平台移植分析

日期：2026-08-05
状态：分析完成，待实施

## 背景

项目内置的 agent 写剧本功能（skill_agent / Harness）在 Windows 上工作正常，
需要评估移植到 Android / Linux 的可行性与改造点。

## 现状：Windows 依赖极少，且已做平台抽象

### 实际 Windows 依赖（仅 1 处核心 + 测试）

- `src-tauri/src/ai_service/skill_agent/command_executor.rs:111-129`
  - 执行命令：Windows 用 `cmd /C`（`#[cfg(windows)]` 分支），其他平台已用 `sh -c`（`#[cfg(not(windows))]` 分支）—— **平台抽象已存在**
  - `raw_arg`（`std::os::windows::process::CommandExt`）——仅 Windows 分支使用，避免 cmd.exe 自动加引号破坏内层引号
  - 测试 `cmd_preserves_quoted_args_with_raw_arg` 里的 `CommandExt`（仅测试代码）

### 其余全部跨平台

- `FileTools`（file_tools.rs）：纯 `std::fs` 操作（read/write/delete/list），`sanitize` 用 `canonicalize_deepest` + base 路径校验 —— 跨平台逻辑正确
- skills 加载、工具定义（tools.rs）、审批流程（PendingApproval 走 Tauri 事件）、core.rs 的调用上下文 —— 纯逻辑，无系统依赖
- `std::process::id()` —— 跨平台

## 移植关注点（3 个）

### 1. 命令执行（已有基础，需完善）

- Linux/Android 分支已用 `sh -c`，但：
  - Android 的 `sh` 是 **mksh（Toybox）**，命令可用性受限（无 python/node 等）
  - Windows `raw_arg` 的引号处理在 POSIX 分支不需要，但需验证 POSIX 分支的命令注入安全（沙箱目录校验已做）
- `execute_command` 在 Android 上的实际价值有限（skill 依赖的外部工具不可用）

### 2. 沙箱路径（跨平台但需注意）

- `FileTools::sanitize` 逻辑跨平台正确
- Android 沙箱目录（`/sdcard/Android/data/.../game_data`）与桌面不同，**scoped storage 权限模型差异**
- Linux 上 `canonicalize` 对符号链接的行为与 Windows 不同，需测试

### 3. 系统能力差异（最大实际差异）

- Android 无 shell 命令行生态
- 用户审批对话框走 Tauri 事件 —— **跨平台已通**
- 截图/屏幕感知等系统能力 Android 不可用 —— 需 feature 门控

## 移植策略（3 个候选方案）

- **方案 A（推荐）：功能裁剪 + 平台门控**
  - `execute_command` Android 上禁用或降级（返回"平台不支持"）
  - `FileTools` 保留，改造成 Android 路径模型（复用 `saf_bridge`）
  - 系统工具（截图等）加 `cfg` 门控
  - 改动最小、风险最低
- **方案 B：完整移植**：Android 上保留命令执行（Toybox sh 能跑基本命令），需适配沙箱目录 + SAF
- **方案 C：抽象系统接口层**：`CommandRunner` trait，各平台实现——最彻底但改动最大

## 待办

- [ ] 深入 Android 沙箱路径适配（saf_bridge 复用）
- [ ] 选定方案后写实施计划

## Android 上 `sh -c` 的问题分析（2026-08-05 补充）

代码分支（`command_executor.rs:121-129`）：
```rust
tokio::process::Command::new("sh")
    .arg("-c")
    .arg(command)
    .current_dir(cwd_path)
    .output()
    .await
```

### 问题 1：Android 没有完整 POSIX shell 生态（功能层面）

- Android 的 `sh` 是 **mksh（Toybox 版）**，非 bash/dash：
  - 只支持基本 POSIX 语法（变量、if/for/管道），**不支持 bash 特性**（`[[ ]]`、数组、进程替换 `<()` 等）
  - skill 的 SKILL.md 若写 bash 语法会直接报错
- **无外部工具生态**：`python/node/git/curl` 不存在，`ls/cat` 等是 Toybox 精简版（参数子集）
- skill 里"运行 python 脚本生成内容"类命令**必失败**——`execute_command` 在 Android 上价值有限

### 问题 2：cwd 未过沙箱校验（安全层面，全平台隐患）

```rust
let cwd_path = if cwd.trim().is_empty() { sandbox_dir } else { PathBuf::from(cwd.trim()) };
```

- `cwd` 参数**没有经过 `FileTools::sanitize` 校验**——LLM 可让 `cwd=/` 或 `cwd=../../..`，命令在沙箱外执行
- Windows 上同样存在此漏洞；Android 上危害更大（FUSE 层路径别名可能使 `canonicalize` 校验失效）
- **修复**：cwd 必须过 `FileTools::sanitize`（全平台受益）

### 问题 3：app 进程 shell 权限受限（Android 特有）

- Tauri app 跑在 app sandbox（uid 隔离）下，`sh -c` 继承 app 权限
- 无法读其他应用数据（`/data/data/其他应用` 无权限）；部分命令需 root——无 root 手机直接失败
- 这既是限制也是安全边界

### 问题 4：输出解码回退策略（兼容层面）

```rust
fn decode_console_output(bytes: &[u8]) -> String {
    // UTF-8 优先，失败回退 GBK
}
```

- Android/Linux 输出是 UTF-8；命令输出非法 UTF-8（cat 二进制）时回退 **GBK** → 乱码
- **修复**：Windows 回退 GBK，Android/Linux 回退 UTF-8 lossy

### 结论与修复方向

| 问题 | 修复 | 平台 |
|------|------|------|
| shell 生态缺失 | Android 禁用/门控 execute_command（返回"平台不支持"或白名单只读命令） | Android |
| cwd 沙箱逃逸 | cwd 过 `FileTools::sanitize` | 全平台 |
| 输出解码 | 平台化回退（Windows: GBK；其他: UTF-8 lossy） | 全平台 |

## root 权限依赖排查（2026-08-05 补充）

**结论：项目本身不依赖 root 权限。**

排查结果：
- **代码进程调用**（全项目仅 1 处）：`utils/system.rs` 的 `open_folder`（explorer/open/xdg-open 打开文件夹）——无需 root，Android 未实现
- **skill_agent 的 execute_command**：LLM 动态生成命令，无固定 root 需求；`sudo`/`su` 在非交互 `sh -c` 下直接失败（无害），Android app 沙箱下 `su` 需手机已 root + 授权弹窗
- **内置技能**（lingchat-script-editor / file-operations / my-first-skill）：纯文件操作（write_file/read_file），无任何命令依赖
- **Tauri 插件**（tauri-plugin-screenshots）：Win32 API / xcap，无进程调用

移植含义：Android 上只需裁剪 execute_command，核心"AI 写剧本"能力（文件操作 + 技能系统）可完整移植；Linux 用 `sh -c` + `xdg-open` 无需提权。
