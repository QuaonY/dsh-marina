# 生成 CNB 回退用的 update.json
#
# 为什么需要它：GitHub 的 releases API 在国内经常要代理，CNB 的 release API
# 又要登录令牌，只有它的公开只读文件路径能用。所以发布时把最新一版的元数据
# 写进仓库的 update.json，客户端查不到 GitHub 时读这个文件。
#
# 用法（在仓库根目录，发布后运行）：
#   pwsh -File installer\write-update-json.ps1 -Tag v0.1.2 -NotesFile notes.md
#
# -NotesFile 省略时用这里给出的一段通用说明。

param(
    [Parameter(Mandatory = $true)][string]$Tag,
    [string]$NotesFile,
    [string]$Repo = "QuaonY/dsh-marina"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {
    $version = $Tag.TrimStart('v')
    $dist = "dist"
    $portable = "dsh-marina-v$version-windows-x64.exe"
    $portablePath = Join-Path $dist $portable
    if (-not (Test-Path $portablePath)) {
        throw "找不到 $portablePath，先运行 build-release.ps1"
    }
    $size = (Get-Item $portablePath).Length
    $shaLine = Get-Content (Join-Path $dist "$portable.sha256") -Raw
    $shaSize = (Get-Item (Join-Path $dist "$portable.sha256")).Length

    $notes = if ($NotesFile -and (Test-Path $NotesFile)) {
        Get-Content $NotesFile -Raw
    } else {
        "## 变化`n`n- 见 GitHub 上的发布说明。`n"
    }

    $base = "https://cnb.cool/$Repo/-/releases/download/$Tag"
    $manifest = [ordered]@{
        tag_name    = $Tag
        html_url    = "https://github.com/$Repo/releases/tag/$Tag"
        body        = $notes.TrimEnd() + "`n"
        assets      = @(
            [ordered]@{
                name                 = $portable
                browser_download_url = "$base/$portable"
                size                 = $size
            },
            [ordered]@{
                name                 = "$portable.sha256"
                browser_download_url = "$base/$portable.sha256"
                size                 = $shaSize
            }
        )
    }

    $json = $manifest | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText((Join-Path $root "update.json"), $json + "`n", [Text.UTF8Encoding]::new($false))
    Write-Host "已写入 update.json（$Tag，$size 字节）"
    Write-Host "记得提交并推送：git add update.json; git commit -m 'Update update.json for $Tag'"
} finally {
    Pop-Location
}
