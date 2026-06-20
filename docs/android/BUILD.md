# Android 构建指南

这份文档说明如何从源码构建 LingChat Android APK。

## 配套文档

- 设计文档：`docs/superpowers/specs/2026-06-17-android-port-design.md`
- 实施计划：`docs/superpowers/plans/2026-06-17-lingchat-android-port.md`

## 一、环境要求

| 工具 | 版本 | 说明 |
| --- | --- | --- |
| **Node.js** | 18+ | 前端构建 |
| **pnpm** | 8+ | 包管理（推荐；npm/yarn 也行，但脚本里写的是 pnpm） |
| **Rust** | 1.70+ | 业务核心 |
| **rustup target** | `aarch64-linux-android` | `rustup target add aarch64-linux-android` |
| **JDK** | **21**（不要用 25，会报 `IllegalArgumentException: 25`） | Android 构建 |
| **Android SDK** | API 34 | 通过 Android Studio SDK Manager 装 |
| **Android NDK** | 26.1.10909125 | 同上 |
| **Gradle** | 8.x | Tauri 脚手架自带 |
| **adb** | 1.0.41+ | 设备调试 |

环境变量（每个新 shell session 都要重设）：
```powershell
$env:JAVA_HOME = "C:\Program Files\Java\jdk-21.0.10"
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
```

## 二、首次构建流程

```powershell
# 1. 克隆代码
git clone https://github.com/你的账号/LingChat.git
cd LingChat

# 2. 安装前端依赖
pnpm install

# 3. 下载情感模型权重（model.onnx.data，约 390 MB）
pnpm model:download
# Linux/macOS 用：
# pnpm model:download:unix

# 4. 初始化 Android scaffold（首次必须）
pnpm tauri android init

# 5. 构建 APK（debug 版）
pnpm android:build:apk
# 或开发模式（热重载，需要连着 USB）：
pnpm android:dev
```

构建产物位置：
```
src-tauri/gen/android/app/build/outputs/apk/arm64/release/app-arm64-release-unsigned.apk
```

## 三、签名（发布 APK 必须）

仓库不含 keystore（密钥不进 git），需要自己生成：

```powershell
# 生成 keystore（一次性）
keytool -genkeypair -v `
  -keystore ling-chat-release.keystore `
  -alias ling-chat `
  -keyalg RSA -keysize 2048 -validity 10000 `
  -storepass <你的密码> -keypass <你的密码>

# 用 apksigner 签名
$apk = "src-tauri\gen\android\app\build\outputs\apk\arm64\release\app-arm64-release-unsigned.apk"
$signed = "ling-chat-v0.1.0-arm64-signed.apk"
apksigner sign --ks ling-chat-release.keystore --ks-pass pass:<密码> --out $signed $apk
apksigner verify --verbose $signed   # 应该输出 "Verifies"
```

⚠️ **绝对不要把 `.keystore` / `.jks` / `.p12` 提交到 git**。一旦泄露立刻作废。

## 四、安装到设备

```powershell
adb install -r ling-chat-v0.1.0-arm64-signed.apk
adb shell am start -n com.noiq.ling-chat/.MainActivity
```

## 五、运行配置

第一次启动 App 后，进入 **设置 → 模型**，配置：
- **对话模型**：填入 DeepSeek / 通义千问 / 任何 OpenAI 兼容 API 的地址和 Key
- **TTS / 翻译**：同上的 URL

所有配置存到 Tauri 的 plugin store（`settings.json`），**重启 App 后生效**。

## 六、目录约定

```
LingChat/
├── src/                       # Vue 3 前端
├── src-tauri/                 # Rust 后端 + Tauri 配置
│   ├── src/api/chat.rs        # 聊天入口
│   ├── src/ai_service/        # 业务核心
│   ├── src/init/mod.rs        # AppState 初始化
│   ├── src/lib.rs             # AppState 结构 + Tauri setup
│   ├── assets/                # bundled 资源（可选）
│   └── gen/android/           # tauri android init 生成
├── data/
│   └── third_party/
│       └── emotion_model_19emo/    # 情感模型
│           ├── model.onnx          # 模型图结构（仓库含）
│           ├── model.onnx.data     # 模型权重（脚本下载）
│           └── *.json / vocab.txt  # 配置文件（仓库含）
└── scripts/                   # 下载脚本
    ├── download_emotion_model.ps1
    └── download_emotion_model.sh
```

## 七、已知限制

- **桌宠（PetMode）**：UI 入口保留但功能未实现（按设计文档要求）
- **屏幕感知（截图/视觉）**：UI 入口保留但功能未实现
- **本地 LLM**：不实现，全部走 HTTP API
- **Proactive chat 后台服务**：桌面端是 Python 后台；Android 端改为 Kotlin Foreground Service（计划中）

## 八、故障排查

| 症状 | 排查方向 |
| --- | --- |
| Gradle 报 `IllegalArgumentException: 25` | JDK 版本错了，必须 21 |
| `apksigner verify` 不输出 Verifies | 签名 keystore / 密码不对 |
| 白屏 `Failed to request http://tauri.localhost/...` | 资源没打进 bundle；检查 `tauri.conf.json` 的 `bundle.icon` 和 assets |
| 启动即崩 `AndroidRuntime FATAL` | `adb logcat -s AndroidRuntime:E` 看堆栈 |
| 聊天没回复 `[AIDialogueEvent] LLM 未配置` | 设置页没选 chat provider 或没填 API key，重启 App |
| 情感分类没生效 | `model.onnx.data` 没下下来，检查 `data/third_party/emotion_model_19emo/` |

完整工作流见 `LingChat_Android移植_经验与工作流.md`（开发者本地文档）。