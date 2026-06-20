# ============================================================
#  下载 ONNX 情感模型（model.onnx.data）
#
#  说明：
#  - model.onnx (91 KB) 已在仓库里
#  - model.onnx.data (~390 MB) 太大不进 PR，靠这个脚本下
#  - 默认从 GitHub Releases 下载，tag 改成你自己的版本
# ============================================================

$ErrorActionPreference = "Stop"

$ModelDir = Join-Path $PSScriptRoot "..\data\third_party\emotion_model_19emo"
$DataFile = Join-Path $ModelDir "model.onnx.data"

# TODO: 替换成你 fork 的 GitHub Releases 下载地址
# 上传 model.onnx.data 到 https://github.com/你的账号/LingChat/releases/tag/v0.1.0
$DownloadUrl = "https://github.com/你的账号/LingChat/releases/download/v0.1.0/model.onnx.data"

if (-not (Test-Path $ModelDir)) {
    New-Item -ItemType Directory -Path $ModelDir -Force | Out-Null
}

if (Test-Path $DataFile) {
    Write-Host "✓ model.onnx.data 已存在，跳过下载：$DataFile"
    exit 0
}

Write-Host "下载 model.onnx.data（约 390 MB）..."
Write-Host "  源：$DownloadUrl"
Write-Host "  目标：$DataFile"

try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $DataFile -UseBasicParsing
    Write-Host "✓ 下载完成"
} catch {
    Write-Error "下载失败：$_`n请检查：1) 网络 2) URL 是否正确 3) GitHub Release 是否公开"
    exit 1
}