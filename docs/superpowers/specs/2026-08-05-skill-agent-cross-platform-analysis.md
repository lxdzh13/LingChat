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
