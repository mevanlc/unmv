param([string]$Root)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $Root
# Import the cmdlet exactly as an original successful Move-Item would.
Set-Content -LiteralPath 'a space' -Value 'payload'
Microsoft.PowerShell.Management\Move-Item -LiteralPath 'a space' -Destination 'b space'
Set-Alias -Name mv -Value Move-Item -Option AllScope
. ./helper.ps1
unmv 'a space' 'b space'
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath 'a space')) { throw 'literal path round trip failed' }
New-Item -ItemType Directory -Path 'destination' | Out-Null
Set-Content -LiteralPath 'first' -Value 'one'
Set-Content -LiteralPath 'second' -Value 'two'
mv first,second destination
unmv first,second destination
if ($LASTEXITCODE -ne 0 -or -not (Test-Path first) -or -not (Test-Path second)) { throw 'source array round trip failed' }
Set-Content -LiteralPath 'b' -Value 'payload'
Remove-Item Alias:mv
function global:mv { throw 'custom function was executed' }
unmv a b
if ($LASTEXITCODE -ne 1 -or (Test-Path a) -or -not (Test-Path b)) { throw 'shadowed mv was not refused' }
unmv --mv-kind powershell a b
if ($LASTEXITCODE -ne 0 -or -not (Test-Path a)) { throw 'explicit override failed' }
Set-Content -LiteralPath 'third' -Value 'three'
Set-Content -LiteralPath 'fourth' -Value 'four'
Microsoft.PowerShell.Management\Move-Item -LiteralPath third,fourth -Destination destination
unmv --mv-kind=powershell third,fourth destination
if ($LASTEXITCODE -ne 0 -or -not (Test-Path third) -or -not (Test-Path fourth)) { throw 'explicit override array handling failed' }
# FileSystem PSDrives should resolve even when the original source is missing.
Remove-Item Function:mv
Set-Alias -Name mv -Value Move-Item -Option AllScope
New-PSDrive -Name UnmvTest -PSProvider FileSystem -Root $Root | Out-Null
Set-Content -LiteralPath 'drive source' -Value 'drive payload'
mv 'UnmvTest:\drive source' 'UnmvTest:\drive destination'
unmv 'UnmvTest:\drive source' 'UnmvTest:\drive destination'
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath 'drive source')) { throw 'FileSystem PSDrive resolution failed' }
Remove-Item Alias:mv
function global:mv { throw 'custom function was executed' }
Microsoft.PowerShell.Management\Move-Item -LiteralPath 'UnmvTest:\drive source' -Destination 'UnmvTest:\drive destination'
unmv --mv-kind powershell 'UnmvTest:\drive source' 'UnmvTest:\drive destination'
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath 'drive source')) { throw 'explicit override PSDrive handling failed' }
