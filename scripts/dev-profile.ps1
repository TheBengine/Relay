[CmdletBinding()]
param(
    [string]$Name = 'review',
    [string]$Exe,
    [switch]$Stop,
    [switch]$Reset,
    [string]$PythonExe,
    [string]$CuaDriver
)

$ErrorActionPreference = 'Stop'
if ($Name -notmatch '^[a-z][a-z0-9-]{0,31}$' -or $Name -match '^(con|prn|aux|nul|com[0-9]|lpt[0-9])$') {
    throw 'Name must be a simple lower-case identifier, such as review-1.'
}
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not $Exe) { $Exe = Join-Path $projectRoot 'dist\relay.exe' }
elseif (-not [IO.Path]::IsPathRooted($Exe)) {
    if ($Exe -match '(^|[\\/])\.\.([\\/]|$)') { throw 'Parent-relative executable paths are refused.' }
    $Exe = Join-Path $projectRoot $Exe
}
if (-not $PythonExe) {
    # This machine's known local interpreter. Do not search an uncontrolled PATH.
    $PythonExe = 'C:\Program Files\Python314\python.exe'
}
foreach ($toolPath in @($PythonExe, (Join-Path $PSScriptRoot 'dev_profile_core.py'))) {
    if ($toolPath -notmatch '^[CDcd]:\\' -or $toolPath.Substring(2).Contains(':') -or
        $toolPath -match '[/<>"|?*\x00-\x1F]') {
        throw 'Use direct local C: or D: tool paths.'
    }
    $parts = $toolPath.Substring(3).Split('\')
    foreach ($part in $parts) {
        if (-not $part -or $part -eq '.' -or $part -eq '..' -or $part.EndsWith('.') -or $part.EndsWith(' ')) {
            throw 'Ambiguous bootstrap tool paths are refused.'
        }
    }
    $driveRoot = [IO.Path]::GetPathRoot($toolPath)
    if ([IO.DriveInfo]::new($driveRoot).DriveType -ne [IO.DriveType]::Fixed) { throw 'Network and non-fixed tool drives are refused.' }
    # Bootstrap only from the operating-system volume. The Python core then
    # checks native volume mappings before accessing any profile/tool on C:/D:.
    if ($driveRoot -ne [IO.Path]::GetPathRoot([Environment]::SystemDirectory)) {
        throw 'The controller and Python must bootstrap from the local Windows volume.'
    }
    # Build the path list lexically, then inspect the root before any child.
    # A leaf-first existence probe could already follow an ancestor junction.
    $components = @($driveRoot)
    $cursor = $driveRoot
    foreach ($part in $parts) {
        $cursor = Join-Path $cursor $part
        $components += $cursor
    }
    foreach ($component in $components) {
        $item = Get-Item -LiteralPath $component -Force -ErrorAction Stop
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Redirected tool paths are refused.' }
        if ($component -ne $cursor -and -not $item.PSIsContainer) { throw 'Bootstrap parent is not a directory.' }
    }
    if ($item.PSIsContainer) { throw 'The bootstrap tool must be an ordinary file.' }
}
$arguments = @('-B', (Join-Path $PSScriptRoot 'dev_profile_core.py'), '--name', $Name,
    '--exe', $Exe, '--project-root', $projectRoot)
if ($Stop) { $arguments += '--stop' }
if ($Reset) { $arguments += '--reset' }
if ($CuaDriver) { $arguments += @('--driver', $CuaDriver) }
# Only the headless Python controller is run here. It launches/closes the GUI
# through Cua Driver's non-activating route, never through Start-Process.
& $PythonExe @arguments
if ($LASTEXITCODE -ne 0) { throw 'The isolated profile action stopped safely; inspect its error and owner record.' }
