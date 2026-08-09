# Claude Code Sync - Windows Installation Script
# Usage: irm https://raw.githubusercontent.com/osen77/cc-session/master/install.ps1 | iex

$ErrorActionPreference = "Stop"

# Configuration
$REPO = "osen77/cc-session"
$INSTALL_DIR = "$env:LOCALAPPDATA\Programs\ccs"
$ASSET_NAME = "ccs-windows-x86_64.zip"
$DEST_PATH = "$INSTALL_DIR\ccs.exe"
$TEMP_DIR = Join-Path ([System.IO.Path]::GetTempPath()) ("ccs-install-" + [guid]::NewGuid())
$ARCHIVE_PATH = Join-Path $TEMP_DIR $ASSET_NAME

Write-Host ""
Write-Host "=====================================================" -ForegroundColor Cyan
Write-Host "  ccs (Claude Code Sync) 安装程序 (Windows)" -ForegroundColor Cyan
Write-Host "=====================================================" -ForegroundColor Cyan
Write-Host ""

# Get latest version
Write-Host "获取最新版本..." -ForegroundColor Cyan

try {
    $release = Invoke-RestMethod "https://api.github.com/repos/$REPO/releases/latest"
    $LATEST_VERSION = $release.tag_name
    Write-Host "  最新版本: $LATEST_VERSION" -ForegroundColor Green
} catch {
    Write-Host "无法获取最新版本。请检查网络连接。" -ForegroundColor Red
    exit 1
}

Write-Host ""

# Check if already installed
$existingPath = Get-Command ccs -ErrorAction SilentlyContinue
if ($existingPath) {
    try {
        $currentVersion = & ccs --version 2>$null
        if ($currentVersion -match '(\d+\.\d+\.\d+)') {
            $CURRENT_VERSION = "v$($matches[1])"
            Write-Host "  当前版本: $CURRENT_VERSION" -ForegroundColor Cyan

            $latestClean = $LATEST_VERSION -replace '^v', ''
            $currentClean = $CURRENT_VERSION -replace '^v', ''

            if ($latestClean -eq $currentClean) {
                Write-Host "已是最新版本" -ForegroundColor Green
                Write-Host ""
                $reinstall = Read-Host "是否重新安装? [y/N]"
                if ($reinstall -notmatch '^[Yy]') {
                    Write-Host "已取消安装。" -ForegroundColor Cyan
                    exit 0
                }
            }
        }
    } catch {}
    Write-Host ""
}

# Create install and temporary directories
Write-Host "创建安装目录..." -ForegroundColor Cyan
New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
New-Item -ItemType Directory -Force -Path $TEMP_DIR | Out-Null

# Download and extract
$DOWNLOAD_URL = "https://github.com/$REPO/releases/download/$LATEST_VERSION/$ASSET_NAME"

Write-Host "下载中..." -ForegroundColor Cyan
Write-Host "  $DOWNLOAD_URL" -ForegroundColor Gray
Write-Host ""

try {
    if (Get-Command Start-BitsTransfer -ErrorAction SilentlyContinue) {
        Start-BitsTransfer -Source $DOWNLOAD_URL -Destination $ARCHIVE_PATH -Description "Downloading ccs"
    } else {
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $DOWNLOAD_URL -OutFile $ARCHIVE_PATH -UseBasicParsing
    }
    Expand-Archive -Path $ARCHIVE_PATH -DestinationPath $TEMP_DIR -Force
    $EXTRACTED_BINARY = Join-Path $TEMP_DIR "ccs.exe"
    if (-not (Test-Path $EXTRACTED_BINARY)) {
        throw "ccs.exe was not found in $ASSET_NAME"
    }
    Copy-Item -Path $EXTRACTED_BINARY -Destination $DEST_PATH -Force
    Write-Host "下载完成" -ForegroundColor Green
} catch {
    Write-Host "下载失败: $_" -ForegroundColor Red
    exit 1
} finally {
    if (Test-Path $TEMP_DIR) {
        Remove-Item -Path $TEMP_DIR -Recurse -Force
    }
}

Write-Host ""

# Add to PATH if needed
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$INSTALL_DIR*") {
    Write-Host "添加到 PATH..." -ForegroundColor Cyan

    $newPath = "$userPath;$INSTALL_DIR"
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")

    # Update current session PATH
    $env:Path = "$env:Path;$INSTALL_DIR"

    Write-Host "  已添加到用户 PATH" -ForegroundColor Green
    Write-Host "  请重新打开终端以使更改生效" -ForegroundColor Yellow
}

Write-Host ""

# Verify installation
Write-Host "验证安装..." -ForegroundColor Cyan

try {
    $version = & $DEST_PATH --version 2>$null
    Write-Host "  $version" -ForegroundColor Green
} catch {
    Write-Host "安装验证失败" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "=====================================================" -ForegroundColor Cyan
Write-Host "  安装完成！" -ForegroundColor Green
Write-Host "=====================================================" -ForegroundColor Cyan
Write-Host ""

# Check if already configured
try {
    $null = & $DEST_PATH status 2>$null
    Write-Host "已检测到现有配置" -ForegroundColor Green
    Write-Host ""
    Write-Host "常用命令:" -ForegroundColor Cyan
    Write-Host "  ccs sync   - 双向同步"
    Write-Host "  ccs status - 查看状态"
    Write-Host "  ccs update - 检查更新"
} catch {
    Write-Host ""
    $setup = Read-Host "是否立即配置? [Y/n]"

    if ($setup -notmatch '^[Nn]') {
        Write-Host ""
        & $DEST_PATH setup
    } else {
        Write-Host ""
        Write-Host "稍后运行 'ccs setup' 进行配置" -ForegroundColor Cyan
    }
}

Write-Host ""
