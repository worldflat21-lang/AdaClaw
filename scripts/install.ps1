# AdaClaw installer for Windows PowerShell
#
# Usage:
#   irm https://raw.githubusercontent.com/worldflat21-lang/AdaClaw/main/scripts/install.ps1 | iex
#
# Or with a specific version:
#   $env:VERSION = "v0.1.0"
#   irm .../install.ps1 | iex
#
# Environment variables:
#   $env:VERSION      - version to install (default: latest)
#   $env:INSTALL_DIR  - installation directory (default: $env:USERPROFILE\.cargo\bin)

$ErrorActionPreference = "Stop"

# ── Config ────────────────────────────────────────────────────────────────────

$REPO = "worldflat21-lang/AdaClaw"
$BINARY = "adaclaw.exe"
$INSTALL_DIR = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { "$env:USERPROFILE\.cargo\bin" }
$VERSION = if ($env:VERSION) { $env:VERSION } else { "" }

# ── Helpers ───────────────────────────────────────────────────────────────────

function Write-Info  { param($msg) Write-Host "[info] $msg" -ForegroundColor Cyan }
function Write-Ok    { param($msg) Write-Host "[ok]   $msg" -ForegroundColor Green }
function Write-Warn  { param($msg) Write-Host "[warn] $msg" -ForegroundColor Yellow }
function Write-Fail  { param($msg) Write-Host "[error] $msg" -ForegroundColor Red; exit 1 }

# ── Main ──────────────────────────────────────────────────────────────────────

Write-Host ""
Write-Host "  +==========================================+" -ForegroundColor Cyan
Write-Host "  |  AdaClaw Installer                      |" -ForegroundColor Cyan
Write-Host "  |  Lightweight Rust AI Agent Runtime      |" -ForegroundColor Cyan
Write-Host "  +==========================================+" -ForegroundColor Cyan
Write-Host ""

# Detect architecture
$ARCH = (Get-CimInstance Win32_Processor).AddressWidth
if ($ARCH -ne 64) {
    Write-Fail "Only 64-bit Windows is supported."
}
$PLATFORM = "windows-x86_64"
Write-Info "Detected platform: $PLATFORM"

# Get latest version if not specified
if (-not $VERSION) {
    Write-Info "Fetching latest release..."
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$REPO/releases/latest"
        $VERSION = $release.tag_name
    } catch {
        Write-Fail "Could not fetch latest version. Set `$env:VERSION to specify one."
    }
}
Write-Info "Installing AdaClaw $VERSION"

# Build download URL
$ARTIFACT = "adaclaw-$PLATFORM.exe"
$URL = "https://github.com/$REPO/releases/download/$VERSION/$ARTIFACT"

# Create install dir
if (-not (Test-Path $INSTALL_DIR)) {
    New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
    Write-Info "Created directory: $INSTALL_DIR"
}

# Download
Write-Info "Downloading from: $URL"
$DEST = Join-Path $INSTALL_DIR $BINARY
$TMP = [System.IO.Path]::GetTempFileName() + ".exe"

try {
    $ProgressPreference = 'SilentlyContinue'  # Speeds up Invoke-WebRequest significantly
    Invoke-WebRequest -Uri $URL -OutFile $TMP -UseBasicParsing
} catch {
    Remove-Item -Force $TMP -ErrorAction SilentlyContinue
    Write-Fail "Download failed. Check if $VERSION exists: https://github.com/$REPO/releases"
}

# Verify checksum (if available)
$CHECKSUM_URL = "$URL.sha256"
try {
    $checksumContent = Invoke-WebRequest -Uri $CHECKSUM_URL -UseBasicParsing -ErrorAction Stop
    $expectedHash = ($checksumContent.Content -split '\s+')[0].Trim().ToUpper()
    $actualHash = (Get-FileHash -Algorithm SHA256 $TMP).Hash.ToUpper()
    if ($expectedHash -ne $actualHash) {
        Remove-Item -Force $TMP -ErrorAction SilentlyContinue
        Write-Fail "Checksum mismatch! Expected: $expectedHash, Got: $actualHash"
    }
    Write-Ok "Checksum verified"
} catch {
    Write-Warn "Checksum file not available, skipping verification."
}

# Install
Move-Item -Force $TMP $DEST
Write-Ok "Installed to: $DEST"

# Verify
try {
    $ver = & $DEST --version
    Write-Ok "Installation verified: $ver"
} catch {
    Write-Warn "Could not verify installation. Try running: $DEST --version"
}

# Check PATH
$currentPath = [System.Environment]::GetEnvironmentVariable("Path", "User")
if ($currentPath -notlike "*$INSTALL_DIR*") {
    Write-Warn "$INSTALL_DIR is not in your PATH."
    Write-Warn "Adding it now (takes effect in new terminal windows)..."
    $newPath = "$currentPath;$INSTALL_DIR"
    [System.Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    $env:PATH = "$env:PATH;$INSTALL_DIR"
    Write-Ok "PATH updated"
}

Write-Host ""
Write-Host "  ==========================================" -ForegroundColor Green
Write-Host "  ✅  AdaClaw is ready!" -ForegroundColor Green
Write-Host "  ==========================================" -ForegroundColor Green
Write-Host ""
Write-Host "  Get started:"
Write-Host "    adaclaw onboard    # interactive setup wizard"
Write-Host "    adaclaw chat       # start chatting"
Write-Host "    adaclaw doctor     # check configuration"
Write-Host ""
