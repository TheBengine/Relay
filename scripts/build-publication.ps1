# Shared output transaction. Never rename running programs or stop processes.
# All existing destinations remain exclusively held from preflight through copy
# and rollback. An unavailable second destination cannot leave the first changed.
function Get-RelayDirectPath {
    param([Parameter(Mandatory=$true)][string]$Path)
    $raw = $Path
    if ($raw.StartsWith('\\?\')) { $raw = $raw.Substring(4) }
    if ($raw -notmatch '^(?i)[cd]:[\\/]') { throw 'Build paths must use the permitted local C: or D: drive.' }
    $full = [IO.Path]::GetFullPath($raw)
    $driveRoot = [IO.Path]::GetPathRoot($full)
    if ([IO.DriveInfo]::new($driveRoot).DriveType -ne [IO.DriveType]::Fixed) {
        throw 'Build paths must use a fixed local drive, not a mapped share.'
    }
    $parts = [Collections.Generic.List[string]]::new()
    $part = $full
    while ($part) { $parts.Add($part); $part = [IO.Path]::GetDirectoryName($part) }
    for ($index = $parts.Count - 1; $index -ge 0; $index--) {
        try { $attributes = [IO.File]::GetAttributes($parts[$index]) }
        catch [IO.FileNotFoundException] { continue }
        catch [IO.DirectoryNotFoundException] { continue }
        if ($attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw ('Redirected build path refused: ' + $parts[$index])
        }
        if ($index -gt 0 -and -not ($attributes -band [IO.FileAttributes]::Directory)) {
            throw 'A build path ancestor is not a directory.'
        }
    }
    return $full
}

function Get-RelayStaticRuntimeFlags {
    param([AllowNull()][string]$Flags)
    $value = if ($null -eq $Flags) { '' } else { $Flags }
    $matches = [regex]::Matches($value, '(?<!\S)-C\s*target-feature=(?<features>\S+)')
    foreach ($match in $matches) {
        $features = $match.Groups['features'].Value.Split(',')
        if ($features -contains '-crt-static') { throw 'RUSTFLAGS disables the required static runtime.' }
        if ($features -contains '+crt-static') { return $value }
    }
    return ($value.TrimEnd() + ' -Ctarget-feature=+crt-static').TrimStart()
}

function Publish-RelayFiles {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory=$true)][object[]]$Files,
        [Parameter(Mandatory=$true)][string]$TransactionDirectory
    )
    $transaction = Get-RelayDirectPath $TransactionDirectory
    if ($Files.Count -lt 1 -or $Files.Count -gt 32) { throw 'Publication requires between one and thirty-two exact files.' }
    if ([IO.Directory]::Exists($transaction) -or [IO.File]::Exists($transaction)) {
        throw 'Use a new, private transaction directory for each publication.'
    }
    $records = [Collections.Generic.List[object]]::new()
    $targets = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $created = [Collections.Generic.List[object]]::new()
    $changed = [Collections.Generic.List[object]]::new()
    $committed = $false
    $retainRecovery = $false
    try {
        # Validate all sources and target paths before creating or changing any
        # output. Source handles prevent an input from changing during the copy.
        foreach ($file in $Files) {
            $source = Get-RelayDirectPath ([string]$file.Source)
            $target = Get-RelayDirectPath ([string]$file.Target)
            $transactionPrefix=$transaction.TrimEnd([IO.Path]::DirectorySeparatorChar)+[IO.Path]::DirectorySeparatorChar
            if ($source.StartsWith($transactionPrefix,[StringComparison]::OrdinalIgnoreCase) -or $target.StartsWith($transactionPrefix,[StringComparison]::OrdinalIgnoreCase) -or $target.Equals($transaction,[StringComparison]::OrdinalIgnoreCase)) {
                throw 'The rollback directory must be separate from published inputs and outputs.'
            }
            if ($source.Equals($target, [StringComparison]::OrdinalIgnoreCase) -or -not $targets.Add($target)) {
                throw 'Publication inputs require distinct source and destination paths.'
            }
            $input = [IO.File]::Open($source, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
            if ($input.Length -gt 256MB) { $input.Dispose(); throw 'Build output exceeds the publication file limit.' }
            $records.Add([pscustomobject]@{ Source=$source; Target=$target; Input=$input; Output=$null; Existed=$false; Backup=$null })
        }
        foreach ($record in $records) {
            try { $attributes = [IO.File]::GetAttributes($record.Target) }
            catch [IO.FileNotFoundException] { continue }
            catch [IO.DirectoryNotFoundException] { continue }
            if ($attributes -band [IO.FileAttributes]::Directory) { throw 'A build destination is a directory.' }
            try {
                $record.Output = [IO.File]::Open($record.Target, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
            } catch {
                throw ('Close Relay before building or packaging. A program file is busy or cannot be written: ' + $record.Target)
            }
            $record.Existed = $true
        }
        # Every existing target has now passed preflight. Hold the handles until
        # commit/rollback; do not close a probe and leave a replacement race.
        [void][IO.Directory]::CreateDirectory($transaction)
        foreach ($record in $records) {
            if ($record.Existed) {
                $record.Backup = Join-Path $transaction ($records.IndexOf($record).ToString() + '.backup')
                $backup = [IO.File]::Open($record.Backup, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
                try { $record.Output.Position=0; $record.Output.CopyTo($backup) } finally { $backup.Dispose() }
            } else {
                [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($record.Target))
                $record.Output = [IO.File]::Open($record.Target, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
                $created.Add($record)
            }
        }
        foreach ($record in $records) {
            $changed.Add($record)
            $record.Input.Position=0; $record.Output.Position=0
            $record.Input.CopyTo($record.Output)
            $record.Output.SetLength($record.Input.Length)
            $record.Output.Flush($true)
        }
        $committed = $true
    } catch {
        $failure = $_
        foreach ($record in $changed) {
            if ($record.Existed) {
                try {
                    $backup = [IO.File]::OpenRead($record.Backup)
                    try {
                        $record.Output.Position=0; $backup.CopyTo($record.Output)
                        $record.Output.SetLength($backup.Length); $record.Output.Flush($true)
                    } finally { $backup.Dispose() }
                } catch { $retainRecovery = $true }
            }
        }
        if ($retainRecovery) { throw ('Publication failed and rollback could not finish. Keep the recovery files at: '+$transaction) }
        throw $failure
    } finally {
        foreach ($record in $records) {
            if ($record.Output) { $record.Output.Dispose() }
            if ($record.Input) { $record.Input.Dispose() }
        }
        if (-not $committed) {
            foreach ($record in $created) {
                [void](Get-RelayDirectPath $record.Target)
                [IO.File]::Delete($record.Target)
            }
        }
        # Explicit own backup files only; never recursively delete a computed tree.
        foreach ($record in $records) {
            if ($record.Backup -and -not $retainRecovery) {
                [void](Get-RelayDirectPath $record.Backup)
                [IO.File]::Delete($record.Backup)
            }
        }
        if (-not $retainRecovery -and [IO.Directory]::Exists($transaction)) {
            [void](Get-RelayDirectPath $transaction)
            [IO.Directory]::Delete($transaction, $false)
        }
    }
}
