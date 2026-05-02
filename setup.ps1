param()
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Write-Step { param($msg) Write-Host "`n>> $msg" -ForegroundColor Cyan }
function Write-Ok   { param($msg) Write-Host "   [OK] $msg" -ForegroundColor Green }
function Write-Skip { param($msg) Write-Host "   [--] $msg" -ForegroundColor DarkGray }
function Write-Warn { param($msg) Write-Host "   [!!] $msg" -ForegroundColor Yellow }
function Write-Fail { param($msg) Write-Host "   [XX] $msg" -ForegroundColor Red }

function Refresh-Path {
    $env:PATH = [System.Environment]::GetEnvironmentVariable('PATH','Machine') + ';' +
                [System.Environment]::GetEnvironmentVariable('PATH','User')
}

if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Warn "Restarting as Administrator..."
    Start-Process powershell "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`"" -Verb RunAs
    exit
}

Write-Host ""
Write-Host "  AWP Setup" -ForegroundColor White
Write-Host "  ==========" -ForegroundColor DarkGray
Write-Host ""

$tempDir = "$env:TEMP\awp_setup"
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

Write-Step "Checking Node.js..."
$nodeOk = $false
try {
    $nodeVer = & node --version 2>$null
    if ($nodeVer -match 'v(\d+)' -and [int]$Matches[1] -ge 18) {
        Write-Ok "Node.js $nodeVer already installed"
        $nodeOk = $true
    } else {
        Write-Warn "Node.js $nodeVer is too old (need v18+), upgrading..."
    }
} catch { Write-Warn "Node.js not found, installing..." }

if (-not $nodeOk) {
    $nodeMsi = "$tempDir\node.msi"
    Write-Host "   Downloading Node.js LTS..."
    $nodeUrl = "https://nodejs.org/dist/v22.13.1/node-v22.13.1-x64.msi"
    Invoke-WebRequest -Uri $nodeUrl -OutFile $nodeMsi -UseBasicParsing
    Write-Host "   Installing Node.js..."
    Start-Process msiexec -ArgumentList "/i `"$nodeMsi`" /qn /norestart" -Wait
    Refresh-Path
    Write-Ok "Node.js installed"
}

Write-Step "Checking Rust..."
$rustOk = $false
try {
    $cargoVer = & cargo --version 2>$null
    if ($cargoVer) {
        Write-Ok "Rust ($cargoVer) already installed"
        $rustOk = $true
    }
} catch { Write-Warn "Rust not found, installing..." }

if (-not $rustOk) {
    $rustupExe = "$tempDir\rustup-init.exe"
    Write-Host "   Downloading rustup..."
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupExe -UseBasicParsing
    Write-Host "   Installing Rust (this takes a few minutes)..."
    Start-Process $rustupExe -ArgumentList "-y --default-toolchain stable --default-host x86_64-pc-windows-msvc" -Wait -NoNewWindow
    Refresh-Path
    $env:PATH += ";$env:USERPROFILE\.cargo\bin"
    Write-Ok "Rust installed"
}

Write-Step "Checking Visual Studio Build Tools (C++ + Windows SDK)..."
$vsOk = $false

$vsPaths = @(
    "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC",
    "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2019\BuildTools\VC\Tools\MSVC",
    "${env:ProgramFiles}\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC",
    "${env:ProgramFiles}\Microsoft Visual Studio\2022\Community\VC\Tools\MSVC",
    "${env:ProgramFiles}\Microsoft Visual Studio\2022\Professional\VC\Tools\MSVC"
)

foreach ($p in $vsPaths) {
    if (Test-Path $p) { $vsOk = $true; Write-Ok "VS Build Tools found at $p"; break }
}

if (-not $vsOk) {
    $vsExe = "$tempDir\vs_buildtools.exe"
    Write-Host "   Downloading Visual Studio Build Tools..."
    Invoke-WebRequest -Uri "https://aka.ms/vs/17/release/vs_buildtools.exe" -OutFile $vsExe -UseBasicParsing
    Write-Host "   Installing C++ Build Tools and Windows SDK (this takes several minutes)..."
    $vsArgs = @(
        "--quiet", "--wait", "--norestart",
        "--add", "Microsoft.VisualStudio.Workload.VCTools",
        "--add", "Microsoft.VisualStudio.Component.Windows11SDK.22000",
        "--add", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
    )
    Start-Process $vsExe -ArgumentList $vsArgs -Wait
    Refresh-Path
    Write-Ok "Visual Studio Build Tools installed"
}

Write-Step "Checking WebView2 Runtime..."
$wv2Key = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
$wv2Key2 = "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
if ((Test-Path $wv2Key) -or (Test-Path $wv2Key2)) {
    Write-Ok "WebView2 Runtime already installed"
} else {
    $wv2Exe = "$tempDir\webview2.exe"
    Write-Host "   Downloading WebView2 Runtime..."
    Invoke-WebRequest -Uri "https://go.microsoft.com/fwlink/p/?LinkId=2124703" -OutFile $wv2Exe -UseBasicParsing
    Write-Host "   Installing WebView2..."
    Start-Process $wv2Exe -ArgumentList "/silent /install" -Wait
    Write-Ok "WebView2 Runtime installed"
}

Write-Step "Installing npm dependencies..."
Push-Location $PSScriptRoot
try {
    Refresh-Path
    & npm install --save-dev "@tauri-apps/cli@^2" 2>&1 | Out-Null
    Write-Ok "npm dependencies installed"
} catch {
    Write-Fail "npm install failed: $_"
} finally {
    Pop-Location
}

Write-Step "Verifying Rust MSVC toolchain..."
try {
    Refresh-Path
    $env:PATH += ";$env:USERPROFILE\.cargo\bin"
    $rustupOutput = & rustup target list --installed 2>&1
    if ($rustupOutput -notmatch "x86_64-pc-windows-msvc") {
        Write-Host "   Adding MSVC target..."
        & rustup target add x86_64-pc-windows-msvc 2>&1 | Out-Null
    }
    Write-Ok "Rust MSVC toolchain ready"
} catch {
    Write-Warn "Could not verify rustup target (may need to reopen terminal)"
}

Write-Host ""
Write-Host "  All dependencies installed!" -ForegroundColor Green
Write-Host ""
Write-Host "  To build the app, run:" -ForegroundColor White
Write-Host "    npx tauri build" -ForegroundColor Yellow
Write-Host ""
Write-Host "  The installer will be at:" -ForegroundColor White
Write-Host "    src-tauri\target\release\bundle\nsis\ui_0.0.1_x64-setup.exe" -ForegroundColor Yellow
Write-Host ""

$null = Read-Host "Press Enter to exit"
