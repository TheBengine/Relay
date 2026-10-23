[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$TaskRoot)
$ErrorActionPreference='Stop'
if($TaskRoot -notmatch '^[CDcd]:\\' -or $TaskRoot.Substring(2).Contains(':')){throw 'Direct local C:/D: test directory required'}
$taskBase=[IO.Path]::GetFullPath($TaskRoot)
$taskCursor=$taskBase.Substring(0,3)
foreach($taskPart in $taskBase.Substring(3).Split('\')){
    if(-not $taskPart){continue}
    $taskCursor=Join-Path $taskCursor $taskPart
    if(Test-Path -LiteralPath $taskCursor){
        $taskItem=Get-Item -LiteralPath $taskCursor -Force
        if(($taskItem.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $taskItem.LinkType){throw 'Redirected fixture path refused'}
    }
}
if(Get-Module -Name NetSecurity){throw 'Run this fixture in a fresh PowerShell; it never imports NetSecurity'}
Import-Module (Join-Path $PSHOME 'Modules\Microsoft.PowerShell.Utility\Microsoft.PowerShell.Utility.psd1') -ErrorAction Stop
$taskFixture=Join-Path $taskBase ('firewall-fixture-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $taskFixture | Out-Null
$taskPrograms=Join-Path $taskFixture 'programs'
$taskSibling=$taskPrograms+'-sibling'
New-Item -ItemType Directory -Path $taskPrograms,$taskSibling | Out-Null
foreach($taskName in @('relay-current.exe','relay-iroh.exe','relay-private.exe','relay-restricted.exe')){
    [IO.File]::WriteAllText((Join-Path $taskPrograms $taskName),'Not executable; an owned firewall fixture only.')
}
$taskOutside=Join-Path $taskSibling 'relay-outside.exe'
[IO.File]::WriteAllText($taskOutside,'Not executable; an owned boundary fixture only.')
$taskSource=Join-Path $PSScriptRoot 'clean-firewall-rules.ps1'
$taskText=[IO.File]::ReadAllText($taskSource)
$taskTokens=$null;$taskErrors=$null
$taskAst=[Management.Automation.Language.Parser]::ParseInput($taskText,[ref]$taskTokens,[ref]$taskErrors)
if($taskErrors.Count){throw 'Cleaner parse failed'}
$taskAdmin=@($taskAst.FindAll({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Is-Administrator'},$true))
if($taskAdmin.Count -ne 1){throw 'Expected one privilege check to replace for this isolated fixture'}
# Only the privilege check is replaced in this owned copy. Every network-security
# command is shadowed below; module autoload is disabled. No elevation is requested.
$taskMocked=$taskText.Substring(0,$taskAdmin[0].Extent.StartOffset)+'function Is-Administrator { return $true }'+$taskText.Substring($taskAdmin[0].Extent.EndOffset)
$taskCleaner=Join-Path $taskFixture 'cleaner-fixture.ps1'
[IO.File]::WriteAllText($taskCleaner,$taskMocked,[Text.UTF8Encoding]::new($false))
$PSModuleAutoloadingPreference='None'
$global:RelayFirewallQaState=$null

function New-QaRule($Name,$File,$Profile='Public',$Protocol='TCP',$Remote=@('Any')) {
    [pscustomobject]@{Name=$Name;Program=$File;Enabled='True';Direction='Inbound';Action='Allow';Profile=$Profile;Protocol=$Protocol;RemoteAddress=$Remote;EdgeTraversalPolicy='Block'}
}
function Reset-QaState {
    $rules=[ordered]@{}
    $rules.stale=New-QaRule 'stale' (Join-Path $taskPrograms 'relay-missing.exe')
    $rules.live=New-QaRule 'live' (Join-Path $taskPrograms 'relay-current.exe')
    $rules.live.EdgeTraversalPolicy='DeferToUser'
    $rules.peer=New-QaRule 'peer' (Join-Path $taskPrograms 'relay-iroh.exe') 'Public' 'UDP'
    $rules.private=New-QaRule 'private' (Join-Path $taskPrograms 'relay-private.exe') 'Private'
    $rules.restricted=New-QaRule 'restricted' (Join-Path $taskPrograms 'relay-restricted.exe') 'Public' 'TCP' @('10.0.0.0/8')
    $rules.outside=New-QaRule 'outside' $taskOutside
    $rules.uncertain=New-QaRule 'uncertain' 'Z:\relay-no-access.exe'
    $global:RelayFirewallQaState=[pscustomobject]@{Rules=$rules;Writes=0;FailWrite=0;FailAfterWrite=0;Events=@()}
}
function Write-QaMutation($Rule,$Kind) {
    $global:RelayFirewallQaState.Writes++
    if($global:RelayFirewallQaState.FailWrite -eq $global:RelayFirewallQaState.Writes){throw 'Simulated firewall write failure'}
    $global:RelayFirewallQaState.Events+=($Kind+':'+$Rule.Name)
}
function Get-CimInstance { param($Filter) @() }
function Get-NetFirewallApplicationFilter {
    [CmdletBinding()]param([Parameter(ValueFromPipeline=$true)]$InputObject,[string]$PolicyStore)
    process {if($null -ne $InputObject){$InputObject}else{@($global:RelayFirewallQaState.Rules.Values)}}
}
function Get-NetFirewallRule {
    [CmdletBinding()]param([Parameter(ValueFromPipeline=$true)]$InputObject,[string]$PolicyStore)
    process {if($null -ne $InputObject){$InputObject}else{@($global:RelayFirewallQaState.Rules.Values)}}
}
function Get-NetFirewallPortFilter {
    [CmdletBinding()]param([Parameter(ValueFromPipeline=$true)]$InputObject)
    process {$InputObject}
}
function Get-NetFirewallAddressFilter {
    [CmdletBinding()]param([Parameter(ValueFromPipeline=$true)]$InputObject)
    process {$InputObject}
}
function Disable-NetFirewallRule {
    [CmdletBinding()]param($InputObject)
    Write-QaMutation $InputObject 'disable';$InputObject.Enabled='False'
    if($global:RelayFirewallQaState.FailAfterWrite -eq $global:RelayFirewallQaState.Writes){throw 'Simulated lost provider response'}
}
function Set-NetFirewallRule {
    [CmdletBinding()]param($InputObject,$Enabled,$EdgeTraversalPolicy)
    if($PSBoundParameters.ContainsKey('EdgeTraversalPolicy')){
        if($EdgeTraversalPolicy -match '^Defer' -and ($InputObject.RemoteAddress -join ',') -ne 'Any'){throw 'Windows rejects deferred edge traversal with address conditions'}
        Write-QaMutation $InputObject 'edge';$InputObject.EdgeTraversalPolicy=$EdgeTraversalPolicy
    } else {Write-QaMutation $InputObject 'enable';$InputObject.Enabled=$Enabled}
    if($global:RelayFirewallQaState.FailAfterWrite -eq $global:RelayFirewallQaState.Writes){throw 'Simulated lost provider response'}
}
function Set-NetFirewallAddressFilter {
    [CmdletBinding()]param($InputObject,$RemoteAddress)
    if($InputObject.EdgeTraversalPolicy -match '^Defer' -and ($RemoteAddress -join ',') -ne 'Any'){throw 'Windows rejects address conditions with deferred edge traversal'}
    Write-QaMutation $InputObject 'scope';$InputObject.RemoteAddress=@($RemoteAddress | ForEach-Object {if($_ -eq '100.64.0.0/10'){'100.64.0.0/255.192.0.0'}else{$_}})
    if($global:RelayFirewallQaState.FailAfterWrite -eq $global:RelayFirewallQaState.Writes){throw 'Simulated lost provider response'}
}
function Assert-Qa($Value,[string]$Message){if(-not $Value){throw $Message}}
function Invoke-QaCleaner([switch]$Apply) {
    $journal=Join-Path $taskFixture ([guid]::NewGuid().ToString('N'))
    $parameters=@{JournalDirectory=$journal;AllowedRoots=@($taskPrograms)}
    if($Apply){$parameters.Apply=$true}
    $result=(& $taskCleaner @parameters | Out-String) | ConvertFrom-Json
    return $result
}

$taskChecks=@()
try {
    Reset-QaState
    $audit=Invoke-QaCleaner
    Assert-Qa ($audit.Status -eq 'audit' -and $audit.StaleRules -eq 1 -and $audit.InboundScopes -eq 1) 'Audit plan did not match owned fixtures'
    Assert-Qa ($global:RelayFirewallQaState.Writes -eq 0) 'Audit attempted a mutation'
    Assert-Qa ($audit.UncertainSkipped -eq 2 -and $audit.Preserved -eq 1) 'Uncertain path, root boundary or peer UDP was not preserved'
    $taskChecks+='Audit read-only; exact root boundary, uncertain drive, peer UDP and private/restricted rules preserved'

    $result=Invoke-QaCleaner -Apply
    Assert-Qa ($result.Status -eq 'applied') 'Fixture apply failed'
    Assert-Qa ($global:RelayFirewallQaState.Rules.stale.Enabled -eq 'False') 'Stale rule not disabled'
    Assert-Qa (($global:RelayFirewallQaState.Rules.live.RemoteAddress -join ',') -eq 'LocalSubnet,100.64.0.0/255.192.0.0,fd7a:115c:a1e0::/48') 'Live scope was incorrect'
    Assert-Qa ($global:RelayFirewallQaState.Rules.live.EdgeTraversalPolicy -eq 'Block') 'Deferred edge traversal remained on the restricted rule'
    & $taskCleaner -RestoreJournal $result.Journal -JournalDirectory (Split-Path $result.Journal) -AllowedRoots @($taskPrograms) | Out-Null
    Assert-Qa ($global:RelayFirewallQaState.Rules.stale.Enabled -eq 'True') 'Stale rule exact rollback failed'
    Assert-Qa (($global:RelayFirewallQaState.Rules.live.RemoteAddress -join ',') -eq 'Any') 'Live scope exact rollback failed'
    Assert-Qa ($global:RelayFirewallQaState.Rules.live.EdgeTraversalPolicy -eq 'DeferToUser') 'Original deferred edge traversal was not restored'
    $taskChecks+='Real cleaner apply and journal restore against in-memory NetSecurity passed'

    Reset-QaState
    $global:RelayFirewallQaState.FailWrite=2
    $failed=$false
    try{Invoke-QaCleaner -Apply | Out-Null}catch{$failed=$true}
    Assert-Qa $failed 'Injected failure was swallowed'
    Assert-Qa ($global:RelayFirewallQaState.Rules.stale.Enabled -eq 'True') 'Partial apply did not roll back the first write'
    Assert-Qa (($global:RelayFirewallQaState.Rules.live.RemoteAddress -join ',') -eq 'Any') 'Failed scope changed the fixture'
    $taskChecks+='Failure after first mutation restores preceding writes'

    Reset-QaState
    $global:RelayFirewallQaState.FailAfterWrite=2
    $failed=$false
    try{Invoke-QaCleaner -Apply | Out-Null}catch{$failed=$true}
    Assert-Qa $failed 'Lost-response failure was swallowed'
    Assert-Qa ($global:RelayFirewallQaState.Rules.stale.Enabled -eq 'True') 'Lost response did not roll back the stale rule'
    Assert-Qa (($global:RelayFirewallQaState.Rules.live.RemoteAddress -join ',') -eq 'Any') 'A mutation with a lost response escaped rollback'
    Assert-Qa ($global:RelayFirewallQaState.Rules.live.EdgeTraversalPolicy -eq 'DeferToUser') 'Lost edge response escaped rollback'
    $taskChecks+='Mutation completed but provider response lost: both intended writes rolled back'

    Reset-QaState
    $global:RelayFirewallQaState.FailAfterWrite=3
    $failed=$false
    try{Invoke-QaCleaner -Apply | Out-Null}catch{$failed=$true}
    Assert-Qa $failed 'Lost scope response was swallowed'
    Assert-Qa (($global:RelayFirewallQaState.Rules.live.RemoteAddress -join ',') -eq 'Any') 'Restricted scope was not restored before the edge policy'
    Assert-Qa ($global:RelayFirewallQaState.Rules.live.EdgeTraversalPolicy -eq 'DeferToUser') 'Deferred edge policy was not restored after Any'
    Assert-Qa ($global:RelayFirewallQaState.Rules.stale.Enabled -eq 'True') 'Scope response failure left the stale rule disabled'
    $taskChecks+='Lost scope response restores Any before deferred edge traversal, matching actual Windows restrictions'

    Reset-QaState
    $result=Invoke-QaCleaner -Apply
    $global:RelayFirewallQaState.Rules.live.Program=$taskOutside
    $beforeWrites=$global:RelayFirewallQaState.Writes
    $failed=$false
    try{& $taskCleaner -RestoreJournal $result.Journal -JournalDirectory (Split-Path $result.Journal) -AllowedRoots @($taskPrograms) | Out-Null}catch{$failed=$true}
    Assert-Qa $failed 'Changed rule ownership was not rejected'
    Assert-Qa ($global:RelayFirewallQaState.Writes -eq $beforeWrites) 'Ownership mismatch performed a write'
    $taskChecks+='Rollback refuses changed rule ownership before mutation'
    [pscustomobject]@{Status='passed';Checks=$taskChecks;Fixture=$taskFixture;ActualFirewallChanges=0;AdministratorApprovalRequested=$false} | ConvertTo-Json -Depth 6
} finally {
    Remove-Variable -Name RelayFirewallQaState -Scope Global
}
