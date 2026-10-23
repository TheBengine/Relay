# Resolve configured Go inputs before an expensive application build begins.
# Callers load build-publication.ps1 first for the shared local-path guard.
function Resolve-RelayGoBuildSettings {
    [CmdletBinding()]
    param([string]$GoExecutable, [string]$GoModuleCache, [string]$GoBuildCache)
    if ($GoExecutable) {
        $executable = Get-RelayDirectPath $GoExecutable
    } else {
        $executable = $null
        # Inspect only direct local C:/D: PATH entries, never other drives,
        # network paths, redirected directories or recursively discovered tools.
        foreach ($entry in ($env:PATH -split ';')) {
            $directory = $entry.Trim().Trim('"')
            if ($directory -notmatch '^(?i)[cd]:[\\/]') { continue }
            try { $candidate = Get-RelayDirectPath (Join-Path $directory 'go.exe') }
            catch { continue }
            if ([IO.File]::Exists($candidate)) { $executable = $candidate; break }
        }
        if (-not $executable) {
            throw 'Go was not found on a permitted local PATH. Pass -GoExecutable with the prepared go.exe path.'
        }
    }
    if (-not [IO.File]::Exists($executable) -or [IO.Path]::GetExtension($executable) -ine '.exe') {
        throw 'GoExecutable must name an existing direct local go.exe executable.'
    }
    if (-not $GoModuleCache) { $GoModuleCache = $env:GOMODCACHE }
    if (-not $GoBuildCache) { $GoBuildCache = $env:GOCACHE }
    if (-not $GoModuleCache -or -not $GoBuildCache) {
        throw 'Pass -GoModuleCache and -GoBuildCache, or explicitly configure GOMODCACHE and GOCACHE. No cache paths are guessed.'
    }
    $moduleCache = Get-RelayDirectPath $GoModuleCache
    $buildCache = Get-RelayDirectPath $GoBuildCache
    if (-not [IO.Directory]::Exists($moduleCache)) {
        throw 'GoModuleCache must be an existing, prepared dependency directory. This build does not download missing modules.'
    }
    if ([IO.File]::Exists($buildCache)) { throw 'GoBuildCache must be a directory path.' }
    [pscustomobject]@{ Executable=$executable; ModuleCache=$moduleCache; BuildCache=$buildCache }
}
