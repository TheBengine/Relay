# A Windows PowerShell child may inherit PowerShell Core module paths from its
# parent terminal. Load these built-ins from this shell, without changing env.
foreach ($relayRuntimeModuleName in @('Microsoft.PowerShell.Management', 'Microsoft.PowerShell.Utility')) {
    $relayRuntimeManifest = [IO.Path]::Combine($PSHOME, 'Modules', $relayRuntimeModuleName, $relayRuntimeModuleName + '.psd1')
    if (-not [IO.File]::Exists($relayRuntimeManifest)) {
        throw "The running PowerShell installation is missing $relayRuntimeModuleName."
    }
    $relayRuntimeLoadedModule = Get-Module -Name $relayRuntimeModuleName
    if (@($relayRuntimeLoadedModule).Count -ne 1 -or $relayRuntimeLoadedModule.Path -ne $relayRuntimeManifest) {
        Import-Module -Name $relayRuntimeManifest -Force -ErrorAction Stop
    }
}
