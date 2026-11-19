# 打包 Windows 发布物：便携 exe + NSIS 安装包 + 校验文件
#
# 用法（在仓库根目录）：
#   pwsh -File installer\build-release.ps1              # 版本取自 Cargo.toml
#   pwsh -File installer\build-release.ps1 -SkipBuild   # 复用已有的 target\release
#
# 产物在 dist\ 下：
#   dsh-marina-v<版本>-windows-x64.exe            便携版
#   dsh-marina-setup-v<版本>-windows-x64.exe      安装包
#   同名的 .sha256 各一份

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
    if (-not $version) { throw "无法从 Cargo.toml 读取版本号" }
    Write-Host "打包 DSH Marina $version"

    if (-not $SkipBuild) {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build --release 失败" }
    }

    $exe = "target\release\dsh-marina.exe"
    if (-not (Test-Path $exe)) { throw "找不到 $exe，先构建发布版" }

    $dist = "dist"
    New-Item -ItemType Directory -Force -Path $dist | Out-Null
    Get-ChildItem $dist -Filter "dsh-marina-*$version*" | Remove-Item -Force

    # 便携版
    $portable = "dsh-marina-v$version-windows-x64.exe"
    Copy-Item $exe (Join-Path $dist $portable) -Force
    $hash = (Get-FileHash (Join-Path $dist $portable) -Algorithm SHA256).Hash.ToLower()
    "$hash  $portable" | Set-Content -Path (Join-Path $dist "$portable.sha256") -Encoding ASCII -NoNewline
    Write-Host "便携版 $portable  $((Get-Item (Join-Path $dist $portable)).Length) 字节"

    # 安装包
    $candidates = @(
        (Join-Path ${env:ProgramFiles(x86)} "NSIS\makensis.exe"),
        (Join-Path $env:ProgramFiles "NSIS\makensis.exe")
    )
    $makensis = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $makensis) {
        throw "找不到 makensis.exe。安装：winget install NSIS.NSIS"
    }
    Push-Location "installer"
    try {
        & $makensis "/DVERSION=$version" "dsh-marina.nsi"
        if ($LASTEXITCODE -ne 0) { throw "makensis 失败" }
    } finally {
        Pop-Location
    }
    $setup = "dsh-marina-setup-v$version-windows-x64.exe"
    Move-Item (Join-Path "installer" $setup) (Join-Path $dist $setup) -Force
    $setupHash = (Get-FileHash (Join-Path $dist $setup) -Algorithm SHA256).Hash.ToLower()
    "$setupHash  $setup" | Set-Content -Path (Join-Path $dist "$setup.sha256") -Encoding ASCII -NoNewline
    Write-Host "安装包 $setup  $((Get-Item (Join-Path $dist $setup)).Length) 字节"

    Write-Host ""
    Write-Host "SHA-256："
    Get-ChildItem $dist -Filter "*.sha256" | ForEach-Object {
        Write-Host ("  " + (Get-Content $_.FullName -Raw))
    }
    Write-Host ""
    Write-Host "别忘了：发布前把 update.json 的 tag_name、html_url、body 和 assets 更新到这个版本。"
} finally {
    Pop-Location
}
