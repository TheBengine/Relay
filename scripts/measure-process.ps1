[CmdletBinding()]
param(
    [string]$ProcessName = 'relay',
    [int[]]$ProcessId,
    [ValidateRange(1, 300)][int]$Seconds = 30,
    [string]$OutputCsv
)
$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$initial = if ($ProcessId) { @(Get-Process -Id $ProcessId -ErrorAction Stop) } else { @(Get-Process -Name $ProcessName -ErrorAction Stop) }
$samples = foreach ($process in $initial) {
    [PSCustomObject]@{ Id = $process.Id; CPU = $process.TotalProcessorTime.TotalSeconds; Time = [DateTime]::UtcNow }
}
Start-Sleep -Seconds $Seconds
$results = foreach ($sample in $samples) {
    $current = Get-Process -Id $sample.Id -ErrorAction SilentlyContinue
    if (-not $current) { continue }
    $elapsed = ([DateTime]::UtcNow - $sample.Time).TotalSeconds
    $cpuDelta = $current.TotalProcessorTime.TotalSeconds - $sample.CPU
    [PSCustomObject]@{
        UTC = [DateTime]::UtcNow.ToString('o')
        Name = $current.ProcessName
        PID = $current.Id
        SampleSeconds = [Math]::Round($elapsed, 3)
        WorkingSetMiB = [Math]::Round($current.WorkingSet64 / 1MB, 2)
        PrivateMemoryMiB = [Math]::Round($current.PrivateMemorySize64 / 1MB, 2)
        CPUSeconds = [Math]::Round($cpuDelta, 4)
        CPUPercentOfOneCore = [Math]::Round(100 * $cpuDelta / $elapsed, 2)
        CPUPercentOfMachine = [Math]::Round(100 * $cpuDelta / $elapsed / [Environment]::ProcessorCount, 2)
        LogicalProcessors = [Environment]::ProcessorCount
    }
}
$results | Format-Table -AutoSize
if ($OutputCsv) {
    $results | Export-Csv -LiteralPath $OutputCsv -NoTypeInformation -Encoding UTF8
    Write-Host "Saved $OutputCsv"
}
