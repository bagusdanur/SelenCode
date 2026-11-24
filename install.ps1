# Installer satu baris untuk SelenCode di Windows (PowerShell / Windows Terminal)
# Pakai:
#   irm https://raw.githubusercontent.com/bagusdanur/SelenCode/main/install.ps1 | iex

$ErrorActionPreference = "Stop"

$Repo = if ($env:SELENCODE_REPO) { $env:SELENCODE_REPO } else { "bagusdanur/SelenCode" }

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Arsitektur 32-bit belum didukung. SelenCode memerlukan Windows x64."
}

$Arch = $env:PROCESSOR_ARCHITECTURE
if ($Arch -and $Arch -ne "AMD64") {
    throw "Arsitektur Windows '$Arch' belum didukung rilis binary resmi (hanya x64/AMD64)."
}

$Asset = "selen-windows-x64.exe"
$Url = "https://github.com/$Repo/releases/latest/download/$Asset"
$InstallDir = if ($env:SELENCODE_INSTALL_DIR) { $env:SELENCODE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "SelenCode" }
$Target = Join-Path $InstallDir "selen.exe"
$TmpFile = Join-Path $InstallDir ".selen.tmp.$PID.exe"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

Write-Host "Mengunduh SelenCode ($Asset) dari GitHub Releases..." -ForegroundColor Cyan
try {
    Invoke-WebRequest -Uri $Url -OutFile $TmpFile -UseBasicParsing
    if (-not (Test-Path $TmpFile) -or (Get-Item $TmpFile).Length -eq 0) {
        throw "File hasil unduhan kosong."
    }
    Move-Item -Force $TmpFile $Target
} catch {
    if (Test-Path $TmpFile) { Remove-Item -Force $TmpFile -ErrorAction SilentlyContinue }
    throw "Gagal mengunduh/memasang SelenCode dari $Url : $_"
}

# Tambahkan folder install ke User PATH bila belum ada
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @()
if ($UserPath) { $PathEntries = $UserPath -split ';' | Where-Object { $_ -ne '' } }
if ($PathEntries -notcontains $InstallDir) {
    $NewUserPath = ($PathEntries + $InstallDir) -join ';'
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    Write-Host "Ditambahkan ke User PATH: $InstallDir" -ForegroundColor Yellow
}
if (($env:Path -split ';') -notcontains $InstallDir) {
    $env:Path = "$InstallDir;$env:Path"
}

Write-Host ""
try {
    $Ver = & $Target --version
    Write-Host "Berhasil memasang $Ver di $Target" -ForegroundColor Green
} catch {
    Write-Host "Berhasil memasang binary di $Target" -ForegroundColor Green
}

Write-Host ""
Write-Host "Langkah cepat pertama kali:"
Write-Host '  1. Buat config: selencode.toml (salin dari config.example.toml)'
Write-Host '  2. Set API key: $env:SELENCODE_ROUTER_KEY = "api-key-kamu"'
Write-Host '  3. Jalankan:    selen'
