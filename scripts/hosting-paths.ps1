. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
# Shared path policy for the app's per-user community and portable hosting tools.
function ConvertTo-RelayShellPath {
    param([Parameter(Mandatory=$true)][string]$Path)
    # Rust canonicalizes a local Windows path as \\?\C:\..., which Windows
    # PowerShell's filesystem provider cannot use with Join-Path. Only remove
    # the verbatim prefix for an ordinary drive path; preserve other prefixes.
    if($Path.StartsWith('\\?\', [StringComparison]::Ordinal) -and $Path.Length -ge 7 -and
       [char]::IsLetter($Path[4]) -and $Path[5] -eq ':' -and $Path[6] -eq '\'){
        $Path=$Path.Substring(4)
    }
    return [IO.Path]::GetFullPath($Path)
}

function Resolve-RelayDataDirectory {
    param([string]$DataDirectory,[string]$PackageRoot,[switch]$CheckLegacy)
    if($DataDirectory){return ConvertTo-RelayShellPath -Path $DataDirectory}
    if($env:RELAY_SERVER_DATA_DIR){return ConvertTo-RelayShellPath -Path $env:RELAY_SERVER_DATA_DIR}
    if(-not $env:LOCALAPPDATA){throw 'Cannot find your per-user server storage folder; pass -DataDirectory.'}
    $destination=[IO.Path]::Combine((ConvertTo-RelayShellPath -Path $env:LOCALAPPDATA), 'Relay\server-data')
    if($CheckLegacy -and -not(Test-Path -LiteralPath (Join-Path $destination 'relay.sqlite3') -PathType Leaf)){
        $config=if($env:RELAY_CONFIG_DIR){$env:RELAY_CONFIG_DIR}elseif($env:APPDATA){Join-Path $env:APPDATA 'Relay\Relay\config'}else{$null}
        $sources=@()
        if($config){$sources+=Join-Path (ConvertTo-RelayShellPath -Path $config) 'server-data'}
        if($PackageRoot){$sources+=Join-Path $PackageRoot 'data'}
        foreach($source in $sources){
            if(Test-Path -LiteralPath (Join-Path $source 'relay.sqlite3') -PathType Leaf){
                throw "An earlier community exists at $source. Stop its old host, then run scripts\import-community.ps1 -SourceDirectory `"$source`" -OfflineConfirmed to copy it safely into $destination. Your existing community was left in place. Use an explicit -DataDirectory for an intentional fresh host."
            }
        }
    }
    return $destination
}
