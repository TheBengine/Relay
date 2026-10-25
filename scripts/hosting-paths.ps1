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

# Selection records contain only relative UUIDs. Do not follow redirected roots
# while resolving them, including when this helper is used by stop-server.ps1.
function Get-RelaySelectedHostPath {
    param([Parameter(Mandatory=$true)][string]$Path)
    $full=ConvertTo-RelayShellPath -Path $Path
    if($full -notmatch '^[A-Za-z]:\\' -or $full.Substring(2).Contains(':')){throw 'A direct local server path is required.'}
    $drive=[IO.DriveInfo]::new($full.Substring(0,3))
    if($drive.DriveType -ne [IO.DriveType]::Fixed){throw 'Local server storage needs a fixed local drive.'}
    $current=$full.Substring(0,3)
    foreach($part in $full.Substring(3).Split('\')){
        if(-not $part){continue}
        $current=Join-Path $current $part
        if(Test-Path -LiteralPath $current){
            if((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint){throw 'Redirected server storage is not supported.'}
        }
    }
    return $full
}

function Resolve-RelayDataDirectory {
    param([string]$DataDirectory,[string]$PackageRoot,[switch]$CheckLegacy)
    if($DataDirectory){return ConvertTo-RelayShellPath -Path $DataDirectory}
    if($env:RELAY_SERVER_DATA_DIR){
        $destination=ConvertTo-RelayShellPath -Path $env:RELAY_SERVER_DATA_DIR
    } else {
        if(-not $env:LOCALAPPDATA){throw 'Cannot find your per-user server storage folder; pass -DataDirectory.'}
        $destination=[IO.Path]::Combine((ConvertTo-RelayShellPath -Path $env:LOCALAPPDATA), 'Relay\server-data')
    }
    # The app can create a separate community without moving the previous one.
    # Default start/stop tools must resolve the same bounded, relative selection.
    $destination=Get-RelaySelectedHostPath $destination
    $choicePath=Join-Path $destination 'local-server-choice.json'
    if(Test-Path -LiteralPath $choicePath){
        $choicePath=Get-RelaySelectedHostPath $choicePath
        if((Get-Item -LiteralPath $choicePath).Length -gt 16384){throw 'Local server selection is too large.'}
        $choice=Get-Content -LiteralPath $choicePath -Raw | ConvertFrom-Json
        $allowed=@('active','created','last_creation')
        if(@($choice.PSObject.Properties.Name | Where-Object {$_ -notin $allowed}).Count -or
           @($choice.created).Count -gt 64){throw 'Local server selection is invalid.'}
        foreach($id in @($choice.created)){
            if($id -isnot [string] -or $id -cnotmatch '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'){throw 'Local server selection is invalid.'}
        }
        if($null -ne $choice.active){
            if($choice.active -isnot [string] -or @($choice.created) -cnotcontains $choice.active){throw 'Local server selection is invalid.'}
            $selected=Get-RelaySelectedHostPath (Join-Path (Join-Path $destination 'communities') $choice.active)
            if(-not(Test-Path -LiteralPath $selected -PathType Container)){throw 'The selected local server folder is missing. Existing servers were kept.'}
            return $selected
        }
    }
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
