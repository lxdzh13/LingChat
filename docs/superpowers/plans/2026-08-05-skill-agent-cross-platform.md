# skill_agent 跨平台移植实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 skill_agent（AI 剧本写作 Harness）移植到 Android / Linux，修复已知平台问题（cwd 沙箱逃逸、输出解码、Android 命令门控），并通过跨平台编译验证。

**Architecture:** 采用功能裁剪方案（方案 A）：全平台保留文件工具与技能系统（纯 std::fs，已跨平台），修复 cwd 沙箱校验与输出解码两个全平台隐患，Android 上禁用 `execute_command`（shell 生态缺失，Toybox 无外部工具），Linux 上 `sh -c` 分支已存在无需改动。

**Tech Stack:** Rust 2021 / Tauri 2 / tokio process / cfg 平台门控

## Global Constraints

- 分支纪律：只在 `tauri-refactor` 分支工作，提交本地中文信息，推送需用户同意
- 质量检测：每个任务完成后 `cargo check` + `cargo clippy` + `cargo test` + 前端 `vue-tsc`（如有前端改动）
- 沙箱边界：所有 LLM 提供的路径必须过 `FileTools::sanitize`（`canonicalize_deepest` + `starts_with` 校验）
- 平台门控用 `#[cfg(target_os = "...")]`，不引入新依赖
- 改动范围限定 `src-tauri/src/ai_service/skill_agent/`，不动其他模块

---

### Task 1: cwd 沙箱校验（全平台安全修复）

**Files:**
- Modify: `src-tauri/src/ai_service/skill_agent/tools.rs`（execute_tool 的 execute_command 分支）
- Modify: `src-tauri/src/ai_service/skill_agent/command_executor.rs`（execute_command 签名加 allow_any_path 参数）
- Test: 两文件各自的 `#[cfg(test)]` 模块

**Interfaces:**
- Consumes: `SkillAgentRunContext`（含 `config.allow_any_path: bool`）、`FileTools::sanitize`
- Produces: `execute_command` 新增参数 `allow_any_path: bool`；`execute_tool` 在调用前用 `FileTools::sanitize` 校验 cwd

- [ ] **Step 1: 在 command_executor.rs 写失败测试（cwd 越界被拒）**

在 `command_executor.rs` 的 tests 模块追加：

```rust
#[cfg(not(windows))]
#[tokio::test]
async fn cwd_outside_sandbox_is_rejected() {
    // 沙箱根
    let root = std::env::temp_dir().join(format!("lc_ce_test_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    // 沙箱外的目录（临时目录的父级）
    let outside = std::env::temp_dir();

    let channel = tauri::ipc::Channel::new(|_| {});
    let approvals: ApprovalMap = Arc::new(Mutex::new(HashMap::new()));
    // cwd 指向沙箱外
    let result = execute_command(
        &channel,
        &approvals,
        true, // auto_approve，跳过审批
        &root,
        "pwd",
        outside.to_str().unwrap(),
        false, // allow_any_path = false
    )
    .await;
    assert!(result.is_err(), "cwd 越界应被拒绝");
    let _ = std::fs::remove_dir_all(&root);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib ai_service::skill_agent::command_executor 2>&1 | grep cwd_outside`
Expected: FAIL（`allow_any_path` 参数不存在，编译错误）

- [ ] **Step 3: 修改 execute_command 签名加 allow_any_path**

在 `command_executor.rs` 的 `execute_command` 签名追加参数：

```rust
pub async fn execute_command(
    channel: &tauri::ipc::Channel<SkillAgentEvent>,
    approvals: &ApprovalMap,
    auto_approve: bool,
    sandbox_dir: &Path,
    command: &str,
    cwd: &str,
    allow_any_path: bool, // 新增：是否允许沙箱外 cwd
) -> anyhow::Result<CommandOutput> {
```

在函数体开头（审批逻辑之后、构造 cwd_path 之前）加校验：

```rust
// cwd 必须过沙箱校验（与 FileTools::sanitize 同一逻辑），防 LLM 把命令
// 引到沙箱外执行（如 cwd=/ 或 ../../..）
let cwd_path = if cwd.trim().is_empty() {
    sandbox_dir.to_path_buf()
} else {
    let raw = std::path::PathBuf::from(cwd.trim());
    if !allow_any_path {
        let ft = crate::ai_service::skill_agent::file_tools::FileTools {
            sandbox_dir: sandbox_dir.to_path_buf(),
            allow_any_path: false,
        };
        ft.sanitize(cwd)?;
    }
    raw
};
```

删除原有的 `cwd_path` 构造代码（在 `#[cfg(windows)]` 之前的那段）。

- [ ] **Step 4: 修改 tools.rs 传 allow_any_path**

在 `tools.rs` 的 execute_command 分支，调用处传入 `ctx.config.allow_any_path`：

```rust
match command_executor::execute_command(
    &ctx.channel,
    &ctx.approvals,
    ctx.config.auto_approve_commands,
    &ctx.sandbox_dir,
    command,
    cwd,
    ctx.config.allow_any_path,
)
.await
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --lib ai_service::skill_agent`
Expected: PASS（新增测试 + 既有测试全过）

- [ ] **Step 6: 质量检测 + 提交**

Run: `cargo check && cargo clippy --lib 2>&1 | grep -c "skill_agent"`（应为 0 或仅既有警告）
Run: `cargo test --lib ai_service::skill_agent`
Commit:
```bash
git add src-tauri/src/ai_service/skill_agent/
git commit -m "fix: execute_command 的 cwd 增加沙箱校验，防 LLM 命令逃逸沙箱（全平台）"
```

---

### Task 2: 输出解码平台化（Windows GBK / 其他 UTF-8 lossy）

**Files:**
- Modify: `src-tauri/src/ai_service/skill_agent/command_executor.rs`（decode_console_output）
- Test: 同文件 tests 模块

**Interfaces:**
- Consumes: 无
- Produces: `decode_console_output` 平台化——Windows 回退 GBK，其他平台回退 UTF-8 lossy

- [ ] **Step 1: 写失败测试（非 UTF-8 字节在非 Windows 上不乱码）**

在 `command_executor.rs` 的 tests 模块追加：

```rust
#[cfg(not(windows))]
#[test]
fn non_utf8_decodes_lossy_not_gbk() {
    // 非法 UTF-8 字节（如 0xFF 0xFE），GBK 解码会产出乱码字符
    let bytes = [0xFFu8, 0xFE, 0x41];
    let s = decode_console_output(&bytes);
    // UTF-8 lossy 会替换为 U+FFFD；GBK 会产出中文字符
    assert!(
        s.contains('\u{FFFD}'),
        "非 Windows 平台应 lossy 解码，实际: {:?}",
        s
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib ai_service::skill_agent::command_executor::tests::non_utf8 2>&1 | grep non_utf8`
Expected: FAIL（当前回退 GBK，`0xFF 0xFE` 在 GBK 下是「ÿþ」无 U+FFFD）

- [ ] **Step 3: 平台化 decode_console_output**

替换 `decode_console_output` 实现：

```rust
/// 子进程输出解码：中文 Windows 上命令输出通常是 GBK/CP936，非 UTF-8 时回退 GBK；
/// 其他平台输出是 UTF-8，回退 lossy 替换（cat 二进制等场景不乱码）。
fn decode_console_output(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    #[cfg(target_os = "windows")]
    {
        encoding_rs::GBK.decode(bytes).0.into_owned()
    }
    #[cfg(not(target_os = "windows"))]
    {
        String::from_utf8_lossy(bytes).into_owned()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --lib ai_service::skill_agent`
Expected: PASS

- [ ] **Step 5: 质量检测 + 提交**

Run: `cargo check && cargo clippy --lib 2>&1 | grep "skill_agent"`
Commit:
```bash
git add src-tauri/src/ai_service/skill_agent/command_executor.rs
git commit -m "fix: 命令输出解码平台化——Windows 回退 GBK，其他平台回退 UTF-8 lossy"
```

---

### Task 3: Android 上禁用 execute_command（功能裁剪）

**Files:**
- Modify: `src-tauri/src/ai_service/skill_agent/tools.rs`（execute_tool 的 execute_command 分支加 cfg）
- Test: 无新增（平台门控，Android 编译验证在 Task 4）

**Interfaces:**
- Consumes: 无
- Produces: Android 上 `execute_command` 工具返回"平台不支持"错误

- [ ] **Step 1: 在 tools.rs 的 execute_command 分支加 Android 门控**

替换 `tools.rs` 的 `"execute_command" =>` 分支：

```rust
"execute_command" => {
    #[cfg(target_os = "android")]
    {
        // Android 的 sh 是 Toybox（mksh），无完整 POSIX 工具生态
        // （无 python/node/git 等），execute_command 无实际价值且是安全面，
        // 直接禁用。
        let _ = (ctx, args);
        (
            false,
            "当前平台（Android）不支持执行 shell 命令：系统 shell 缺少完整工具生态。请改用文件工具完成任务。".into(),
        )
    }
    #[cfg(not(target_os = "android"))]
    {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cwd = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        if command.is_empty() {
            return (false, "缺少 command 参数".into());
        }
        match command_executor::execute_command(
            &ctx.channel,
            &ctx.approvals,
            ctx.config.auto_approve_commands,
            &ctx.sandbox_dir,
            command,
            cwd,
            ctx.config.allow_any_path,
        )
        .await
        {
            Ok(out) => (out.exit_code == 0, out.to_prompt_string()),
            Err(e) => (false, e.to_string()),
        }
    }
}
```

- [ ] **Step 2: 质量检测 + 提交（Windows 上编译验证）**

Run: `cargo check && cargo test --lib ai_service::skill_agent`
Expected: PASS（Android 分支在 Windows 编译时被 cfg 排除，不影响）
Commit:
```bash
git add src-tauri/src/ai_service/skill_agent/tools.rs
git commit -m "feat: Android 上禁用 execute_command 工具（Toybox 无完整工具生态，功能裁剪）"
```

---

### Task 4: 跨平台编译验证（Linux + Android）

**Files:**
- 无代码改动，纯验证

**Interfaces:**
- Consumes: Task 1-3 全部改动
- Produces: 跨平台编译通过的验证记录

- [ ] **Step 1: 检查 Rust 交叉编译 target**

Run: `rustup target list --installed | grep -E "linux|android"`
Expected: 应有 `aarch64-linux-android` 等 Android target。若无 `x86_64-unknown-linux-gnu`，执行 `rustup target add x86_64-unknown-linux-gnu`（cargo check 只编译 std，无需 Linux 链接器）

- [ ] **Step 2: Linux 编译验证**

Run: `cargo check --target x86_64-unknown-linux-gnu 2>&1 | grep -E "^error|Finished"`
Expected: Finished（无错误）。若报错，逐条修复（平台相关的 cfg 问题）

- [ ] **Step 3: Android 编译验证**

Run: `cargo check --target aarch64-linux-android 2>&1 | grep -E "^error|Finished"`
Expected: Finished（无错误）。若报错，逐条修复（重点看 skill_agent 相关）

- [ ] **Step 4: Android 真机验证（如果设备在线）**

Run: `adb devices` → 若 `95379e0c` 在线，构建 APK 装真机，验证：
- 剧本编辑器 Agent 正常对话（文件工具可用）
- 调用 execute_command 时返回"平台不支持"提示
- 若设备离线，记录为待验证项

- [ ] **Step 5: 更新分析文档 + 提交**

在 `docs/superpowers/specs/2026-08-05-skill-agent-cross-platform-analysis.md` 追加实施记录：
```markdown
## 实施记录（2026-08-05）

- Task 1: cwd 沙箱校验 ✅（全平台安全修复）
- Task 2: 输出解码平台化 ✅（Windows GBK / 其他 lossy）
- Task 3: Android 禁用 execute_command ✅
- Task 4: 跨平台编译验证 ✅ / ⚠️（记录验证结果）
```
Commit:
```bash
git add docs/superpowers/specs/2026-08-05-skill-agent-cross-platform-analysis.md
git commit -m "docs: skill_agent 跨平台移植实施记录"
```
