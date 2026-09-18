$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$resourceDir = Join-Path $projectRoot 'src-tauri/resources/warp'
$binary = Join-Path $resourceDir 'usque.exe'
$output = Join-Path $resourceDir 'DEPENDENCY-NOTICES.txt'
$metadata = & go version -m $binary
if ($LASTEXITCODE -ne 0) { throw 'Cannot read bundled Go binary metadata' }
$metadata | Set-Content -LiteralPath (Join-Path $resourceDir 'BUILD-INFO.txt') -Encoding utf8
'Licenses and notices from dependencies linked into usque v4.2.1.' | Set-Content -LiteralPath $output -Encoding utf8
$goRoot = & go env GOROOT
Get-Content -LiteralPath (Join-Path $goRoot 'LICENSE') -Raw | Add-Content -LiteralPath $output -Encoding utf8
foreach ($line in $metadata) {
    if ($line -match '^\s*dep\s+(\S+)\s+(\S+)') {
        $moduleName = $Matches[1]
        $moduleVersion = $Matches[2]
        $module = & go mod download -json "$moduleName@$moduleVersion" | ConvertFrom-Json
        if ($LASTEXITCODE -ne 0 -or !$module.Dir) { throw "Cannot download license sources for $moduleName" }
        $licenses = & rg --files --hidden -g '*LICENSE*' -g '*LICENCE*' -g '*NOTICE*' -g '*COPYING*' $module.Dir
        if (!$licenses) { throw "No license found for $moduleName" }
        foreach ($license in $licenses) {
            "`n----- $moduleName@$moduleVersion : $($license.Substring($module.Dir.Length)) -----`n" | Add-Content -LiteralPath $output -Encoding utf8
            Get-Content -LiteralPath $license -Raw | Add-Content -LiteralPath $output -Encoding utf8
        }
        Write-Host "Collected $moduleName"
    }
}
