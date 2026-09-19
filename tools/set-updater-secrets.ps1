param(
    [string]$KeyDirectory = (Join-Path $env:USERPROFILE '.tauri/codex-state-kit')
)
$ErrorActionPreference = 'Stop'
$repo = 'DouDOU-start/codex-state-kit'
$cliCommand = Get-Command gh -ErrorAction SilentlyContinue
$cli = if ($cliCommand) { $cliCommand.Source } else { Join-Path $env:LOCALAPPDATA 'codex-state-kit-tools/gh/bin/gh.exe' }
if (!(Test-Path -LiteralPath $cli)) { throw '请先安装 GitHub CLI 并执行 gh auth login' }
& $cli auth status --hostname github.com
if ($LASTEXITCODE -ne 0) { throw '请先执行 gh auth login --hostname github.com --web，完成浏览器授权后重试' }
$keyFile = Join-Path $KeyDirectory 'updater.key'
$passwordFile = Join-Path $KeyDirectory 'updater.password'
$config = Get-Content -LiteralPath (Join-Path $PSScriptRoot '../src-tauri/tauri.conf.json') -Raw | ConvertFrom-Json
if ([System.IO.File]::ReadAllText("$keyFile.pub").Trim() -ne $config.plugins.updater.pubkey) {
    throw '本机密钥的公钥与应用配置不一致，拒绝上传'
}
# gh encrypts each value with the repository public key before sending it to GitHub.
# Never pass secret values as command-line arguments or write them to the console.
[System.IO.File]::ReadAllText($keyFile) | & $cli secret set TAURI_SIGNING_PRIVATE_KEY --repo $repo
if ($LASTEXITCODE -ne 0) { throw '上传签名私钥失败' }
[System.IO.File]::ReadAllText($passwordFile) | & $cli secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo $repo
if ($LASTEXITCODE -ne 0) { throw '上传签名密码失败' }
& $cli secret list --repo $repo
if ($LASTEXITCODE -ne 0) { throw '验证 Secrets 列表失败' }
Write-Output '更新签名 Secrets 已配置。私钥与密码没有写入仓库。'
