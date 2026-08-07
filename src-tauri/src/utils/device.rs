//! 推理设备选择工具（供 TTS、情绪识别等 ONNX 推理功能复用）。
//!
//! 三个能力：
//! 1. [`InferenceDevice`] 解析/序列化——`parse_device` / `device_to_string`，
//!    在配置字符串（"cpu" / "gpu" / "npu" / "device:<id>"）和枚举间转换。
//! 2. [`list_devices`]——Windows 上 DXGI 枚举 GPU 列表，供用户选择特定显卡
//!    （如游戏占独显时用核显跑推理）。DXGI 枚举顺序与 DirectML device_id 一致。
//! 3. [`read_configured_device`]——从 settings.json 直接读持久化的设备配置，
//!    不依赖 Tauri store 的加载时机（启动早期 store 可能未从磁盘加载）。
//!
//! 所有使用 ONNX Runtime 推理的功能（当前：本地 TTS、情绪识别）都应通过
//! 本模块选择设备，避免各自复制 EP 配置与枚举逻辑。

use sbv2_core::model::InferenceDevice;
use serde::Serialize;
use tauri::{AppHandle, Manager};

/// 解析推理设备字符串："cpu" | "gpu" | "npu" | "device:<id>"。
/// - `gpu`：DirectML（Windows）或 WebGPU（Linux/macOS，Dawn 默认设备）都支持；
/// - `npu` / `device:<id>`：仅 DirectML（Windows，DXGI 枚举）；
/// - 无 GPU 后端（Android / macOS-CoreML）只支持 cpu。
pub fn parse_device(s: &str) -> Result<InferenceDevice, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cpu" => Ok(InferenceDevice::Cpu),
        #[cfg(any(feature = "tts-directml", feature = "tts-webgpu"))]
        "gpu" => Ok(InferenceDevice::Gpu),
        #[cfg(feature = "tts-directml")]
        "npu" => Ok(InferenceDevice::Npu),
        #[cfg(feature = "tts-directml")]
        _ if s.starts_with("device:") => {
            let id: i32 = s["device:".len()..]
                .trim()
                .parse()
                .map_err(|_| format!("无效的设备 id: {}", s))?;
            Ok(InferenceDevice::Specific(id))
        }
        #[cfg(any(feature = "tts-directml", feature = "tts-webgpu"))]
        other => Err(format!(
            "无效的推理设备: {}（可选: cpu/gpu{}）",
            other,
            if cfg!(feature = "tts-directml") {
                "/npu/device:<id>"
            } else {
                ""
            }
        )),
        #[cfg(not(any(feature = "tts-directml", feature = "tts-webgpu")))]
        other => Err(format!("当前平台仅支持 cpu，收到: {}", other)),
    }
}

/// 序列化推理设备为配置字符串（与 [`parse_device`] 互逆）。
pub fn device_to_string(d: InferenceDevice) -> String {
    match d {
        InferenceDevice::Cpu => "cpu".into(),
        InferenceDevice::Gpu => "gpu".into(),
        InferenceDevice::Npu => "npu".into(),
        InferenceDevice::Specific(id) => format!("device:{}", id),
    }
}

/// 可用的 DirectML 推理设备（DXGI 枚举，device_id 与 DirectML 对齐）。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: i32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
}

/// 枚举系统 DirectML 设备（GPU 列表，Windows）。DXGI 枚举顺序与 DirectML
/// device_id 一致（已验证）。返回给调用方供用户选择特定 GPU。
///
/// 按 `(vendor_id, device_id)` 去重——Intel 混合显卡系统会把同一核显枚举
/// 多次（合成/渲染两个入口），去重后只保留 id 最小的。
/// 非 Windows 平台返回空列表。
pub fn list_devices() -> Vec<DeviceInfo> {
    #[cfg(target_os = "windows")]
    {
        use std::collections::HashSet;
        use windows::Win32::Graphics::Dxgi::*;
        let mut devices = Vec::new();
        let mut seen: HashSet<(u32, u32)> = HashSet::new();
        unsafe {
            if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
                for i in 0u32.. {
                    if let Ok(adapter) = factory.EnumAdapters1(i) {
                        if let Ok(desc) = adapter.GetDesc1() {
                            let name = String::from_utf16_lossy(&desc.Description)
                                .trim_end_matches('\0')
                                .to_string();
                            // 跳过软件渲染器（Basic Render Driver），并去重同一物理 GPU
                            if desc.VendorId != 0x1414
                                && seen.insert((desc.VendorId, desc.DeviceId))
                            {
                                devices.push(DeviceInfo {
                                    id: i as i32,
                                    name,
                                    vendor_id: desc.VendorId,
                                    device_id: desc.DeviceId,
                                });
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        devices
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// 从 settings.json 直接读取持久化的推理设备配置。
///
/// `key` 为配置键（如 `features.local_tts_device`）。直接读文件而非通过
/// Tauri store——启动早期 store 可能未从磁盘加载，读文件保证任何阶段可靠。
///
/// 返回 `None` 表示未配置或读取失败（调用方用默认 CPU 即可）。
pub fn read_configured_device(app: &AppHandle, key: &str) -> Option<InferenceDevice> {
    let path = settings_json_path(app)?;
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let raw = json.get(key)?.as_str()?;
    parse_device(raw).ok()
}

/// 定位 settings.json 的路径（app_config_dir/settings.json）。
fn settings_json_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join(crate::config::STORE_FILE))
}

/// 从磁盘 settings.json 读取字符串配置的辅助函数（供其他模块复用，
/// 避免各自重复"读文件 → 解析 JSON → 取 key"）。
pub fn read_settings_string(app: &AppHandle, key: &str) -> Option<String> {
    let path = settings_json_path(app)?;
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get(key)?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_device_cpu() {
        assert_eq!(parse_device("cpu").unwrap(), InferenceDevice::Cpu);
        // 大小写不敏感
        assert_eq!(parse_device("CPU").unwrap(), InferenceDevice::Cpu);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parse_device_gpu_npu_specific_on_windows() {
        assert_eq!(parse_device("gpu").unwrap(), InferenceDevice::Gpu);
        assert_eq!(parse_device("npu").unwrap(), InferenceDevice::Npu);
        assert_eq!(
            parse_device("device:1").unwrap(),
            InferenceDevice::Specific(1)
        );
    }

    #[test]
    fn parse_device_invalid_returns_err() {
        assert!(parse_device("tpu").is_err());
        assert!(parse_device("").is_err());
    }

    #[test]
    fn device_to_string_roundtrip() {
        assert_eq!(device_to_string(InferenceDevice::Cpu), "cpu");
        #[cfg(target_os = "windows")]
        {
            assert_eq!(device_to_string(InferenceDevice::Gpu), "gpu");
            assert_eq!(device_to_string(InferenceDevice::Specific(2)), "device:2");
        }
    }
}
