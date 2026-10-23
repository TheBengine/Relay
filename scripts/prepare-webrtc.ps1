[CmdletBinding()]
param([string]$CacheDirectory, [string]$OutputDirectory, [switch]$PassThru)
$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'shell-runtime.ps1'))
$packageRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$patchRoot = Join-Path $packageRoot 'vendor\relay-frame-crypto'
$patchSource = Join-Path $patchRoot 'frame_crypto_transformer.cc'
$patchHash = '79813011a8cfd41a0ed872a1c30fc4ac64f2fd1baae4e61022e7822a98c3590a'
$baseHash = '5c2349c960bae4f06f71c102f58552d0d09c912a142811acfcadfb7c883cbf58'
if ((Get-FileHash -LiteralPath $patchSource -Algorithm SHA256).Hash -ne $patchHash) {
    throw 'Native cryptor source changed; review and update the pinned patch digest before building.'
}
$fetchArguments = @{PassThru=$true}
if ($CacheDirectory) { $fetchArguments.CacheDirectory = $CacheDirectory }
$base = & (Join-Path $PSScriptRoot 'fetch-webrtc.ps1') @fetchArguments
if (-not $OutputDirectory) { $OutputDirectory = Join-Path (Split-Path (Split-Path $base -Parent) -Parent) 'relay-frame-crypto' }
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
function Assert-DirectPatchPath([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            if (([IO.File]::GetAttributes($current) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Native patch path contains a junction or symbolic link: $current"
            }
        }
        $current = [IO.Path]::GetDirectoryName($current)
    }
}
Assert-DirectPatchPath $outputRoot
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null
# A generation is never reused or overwritten. Failed compilations remain available
# for diagnosis; the verified original tree and archive remain untouched.
$generation = Join-Path $outputRoot ('build-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $generation | Out-Null
$sdk = Join-Path $generation 'sdk'
Copy-Item -LiteralPath $base -Destination $sdk -Recurse
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw 'Install Visual Studio C++ build tools to prepare the native cryptor.' }
$installation = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $installation) { throw 'Visual Studio x64 C++ build tools were not found.' }
$vcvars = Join-Path $installation 'VC\Auxiliary\Build\vcvars64.bat'
if ($vcvars.Contains('"') -or $vcvars.Contains('%') -or $vcvars.Contains('!')) { throw 'Unsafe Visual Studio installation path.' }
# This command only obtains the installed compiler environment. File operations
# remain native PowerShell operations on exact paths in our new generation.
$compilerEnvironment = & $env:ComSpec /d /c ('call "' + $vcvars + '" >nul && set')
if ($LASTEXITCODE -ne 0) { throw 'Visual Studio compiler environment setup failed.' }
$saved = @{}
try {
    foreach ($line in $compilerEnvironment) {
        if ($line -match '^([^=]+)=(.*)$') {
            $name = $Matches[1]; $value = $Matches[2]
            if ($name -in @('PATH','INCLUDE','LIB','LIBPATH','VCToolsInstallDir','WindowsSdkDir','WindowsSDKVersion')) {
                $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
                [Environment]::SetEnvironmentVariable($name, $value, 'Process')
            }
        }
    }
    $compiler = (Get-Command cl.exe -ErrorAction Stop).Source
    $librarian = (Get-Command lib.exe -ErrorAction Stop).Source
    $object = Join-Path $generation 'relay-frame-crypto.obj'
    $flags = @('/nologo','/c','/O2','/MT','/EHsc','/std:c++20','/DNDEBUG','/DWEBRTC_WIN','/DNOMINMAX','/DWIN32_LEAN_AND_MEAN','/DRTC_ENABLE_H265','/DWEBRTC_USE_H264',
        ('/I'+(Join-Path $sdk 'include')),('/I'+(Join-Path $sdk 'include\third_party\abseil-cpp')),('/I'+(Join-Path $sdk 'include\third_party\boringssl\src\include')),('/Fo'+$object),$patchSource)
    & $compiler @flags *> (Join-Path $generation 'compile.log')
    if ($LASTEXITCODE -ne 0) { throw "Native cryptor compilation failed; see $generation\compile.log" }
    $original = Join-Path $base 'lib\webrtc.lib'
    $member = 'obj/api/crypto/frame_crypto_transformer/frame_crypto_transformer.obj'
    $members = & $librarian /nologo /list $original
    if ($LASTEXITCODE -ne 0 -or @($members | Where-Object { $_ -eq $member }).Count -ne 1) { throw 'Expected native cryptor object is missing or duplicated.' }
    $without = Join-Path $generation 'webrtc-without-crypto.lib'
    & $librarian /nologo ("/remove:$member") ("/out:$without") $original *> (Join-Path $generation 'remove.log')
    if ($LASTEXITCODE -ne 0) { throw 'Could not remove the original native cryptor object.' }
    $patchedLibrary = Join-Path $sdk 'lib\webrtc.lib'
    & $librarian /nologo ("/out:$patchedLibrary") $without $object *> (Join-Path $generation 'merge.log')
    if ($LASTEXITCODE -ne 0) { throw 'Could not add the reviewed native cryptor object.' }
    $members = & $librarian /nologo /list $patchedLibrary
    if ($LASTEXITCODE -ne 0 -or @($members | Where-Object { $_ -eq $member }).Count -ne 0 -or @($members | Where-Object { $_ -eq $object }).Count -ne 1) {
        throw 'Native cryptor object replacement could not be verified.'
    }
    $files = [ordered]@{}
    foreach ($file in Get-ChildItem -LiteralPath $sdk -File -Recurse | Sort-Object FullName) {
        Assert-DirectPatchPath $file.FullName
        $relative = $file.FullName.Substring($sdk.Length+1).Replace('\','/')
        $files[$relative] = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($relative -ne 'lib/webrtc.lib') {
            $baseFile = Join-Path $base $relative
            if ($files[$relative] -ne (Get-FileHash -LiteralPath $baseFile -Algorithm SHA256).Hash.ToLowerInvariant()) { throw "Native patch unexpectedly changed $relative" }
        }
    }
    $manifest = [ordered]@{ schema=1; patch='relay-frame-iv-v1'; base_archive_sha256=$baseHash; patch_sha256=$patchHash;
        library_sha256=$files['lib/webrtc.lib']; object_sha256=(Get-FileHash -LiteralPath $object -Algorithm SHA256).Hash.ToLowerInvariant();
        compiler=$compiler; compiler_sha256=(Get-FileHash -LiteralPath $compiler -Algorithm SHA256).Hash.ToLowerInvariant();
        compiler_version=(Get-Item -LiteralPath $compiler).VersionInfo.FileVersion; flags=$flags;
        recipe_sha256=(Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant(); files=$files }
    $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $sdk 'relay-frame-crypto.json') -Encoding UTF8
    Write-Host "Native cryptor replaced and $($files.Count) SDK files verified in an independent generation."
    if ($PassThru) { Write-Output $sdk }
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
