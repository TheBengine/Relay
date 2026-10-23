"""Default: safety regressions with no Relay or GUI lifecycle actions.

Uses virtual metadata plus one native metadata probe of this headless controller.

After integration, explicitly opt in to the REAL native acceptance check:
  python -B scripts/test-dev-profile.py --lifecycle --exe C:\\...\\dist\\relay.exe
That mode opens one isolated background Relay window through Cua Driver, then
closes and resets only its verified profile. It is never run by the default tests.
"""
import argparse
import ast
import base64
import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import MagicMock, patch

HERE = Path(__file__).absolute().parent
spec = importlib.util.spec_from_file_location('dev_profile_core', HERE / 'dev_profile_core.py')
core = importlib.util.module_from_spec(spec)
spec.loader.exec_module(core)


@unittest.skipUnless(os.name == 'nt', 'The helper uses Windows path semantics.')
class SafetyTests(unittest.TestCase):
    def setUp(self):
        # Lexical synthetic paths only: these paths are never opened or created.
        self.temp = Path(r'C:\r03-pure-tests\Temp')
        self.base, self.profile = core.profile_location(self.temp, 'review-1')
        self.exe = Path(r'C:\r03-pure-tests\dist\relay.exe')
        self.manager = {'schema': 1, 'root': str(self.base), 'nonce': 'a' * 64}
        self.marker = {'schema': 1, 'name': 'review-1', 'profile': str(self.profile),
                       'manager_nonce': 'a' * 64, 'exe': str(self.exe),
                       'client': {'pid': 77, 'created': 123456, 'nonce': 'b' * 64},
                       'launch_pending': False}
        self.process = {'pid': 77, 'created': 123456, 'exe': str(self.exe),
                        'arguments': [str(self.exe), '--profile-dir', str(self.profile),
                                      '--background', '--dev-profile-owner', 'b' * 64]}

    def test_names_cannot_escape_or_alias_device_names(self):
        for name in ('', '.', '..', '../review', r'a\b', 'C:\\review', 'a b', 'Review',
                     'review.', 'review ', 'nul', 'com1', 'lpt9', 'a' * 33):
            with self.subTest(name=name), self.assertRaises(ValueError):
                core.validate_name(name)
        self.assertEqual(core.validate_name('review-1'), 'review-1')

    def test_only_unambiguous_local_absolute_paths(self):
        invalid = (r'E:\data', r'\\server\share', r'\\?\C:\data', 'C:review',
                   r'C:\safe\..\outside', r'C:\safe\.\review', r'C:\safe\\review',
                   'C:/safe', 'C:\\safe\\', r'C:\file:stream', r'C:\name.',
                   r'D:\name ', r'C:\wild*', r'C:\question?', r'C:\con\file',
                   r'C:\NUL.bin', r'C:\CONIN$\driver', r'C:\COM¹\file')
        for path in invalid:
            with self.subTest(path=path), self.assertRaises(ValueError):
                core.local_absolute(path)
        self.assertEqual(core.local_absolute('C:\\'), Path('C:\\'))
        self.assertEqual(core.local_absolute(r'D:\Temp\review'), Path(r'D:\Temp\review'))
        with self.assertRaises(ValueError):
            core.profile_location('C:\\', 'review')

    def test_owner_is_bound_to_exact_temp_child(self):
        core.validate_owner(self.base, self.profile, 'review-1', self.manager, self.marker, self.temp)
        for field, value in (('schema', 2), ('profile', str(self.base)), ('name', 'other'),
                             ('manager_nonce', 'c' * 64)):
            marker = dict(self.marker, **{field: value})
            with self.subTest(field=field), self.assertRaises(ValueError):
                core.validate_owner(self.base, self.profile, 'review-1', self.manager, marker, self.temp)
        with self.assertRaises(ValueError):
            core.validate_owner(self.base, self.profile, 'review-1', self.manager, self.marker,
                                Path(r'D:\another-temp'))

    def test_reset_rejects_arbitrary_base_before_read_or_delete(self):
        with patch.object(core, 'read_json') as read, patch.object(core, 'PinnedTree') as tree:
            with self.assertRaises(ValueError):
                core.reset_profile(self.base.parent, self.profile, 'review-1', self.marker, [], self.temp)
            read.assert_not_called()
            tree.assert_not_called()

    def test_direct_rejects_network_and_substituted_volume_before_filesystem_walk(self):
        for drive_type, device in ((4, r'\Device\LanmanRedirector\server'),
                                   (3, r'\??\E:\redirected')):
            kernel = MagicMock()
            kernel.GetDriveTypeW.return_value = drive_type
            def query(_drive, output, _size):
                output.value = device
                return len(device)
            kernel.QueryDosDeviceW.side_effect = query
            with patch.object(core.ctypes, 'WinDLL', return_value=kernel), \
                    patch.object(core.Path, 'lstat') as walk:
                with self.assertRaises(ValueError):
                    core.direct(self.profile)
                walk.assert_not_called()

    def test_direct_rejects_reparse_and_multilink_file_metadata(self):
        kernel = MagicMock()
        kernel.GetDriveTypeW.return_value = 3
        def query(_drive, output, _size):
            output.value = r'\Device\HarddiskVolume3'
            return len(output.value)
        kernel.QueryDosDeviceW.side_effect = query
        for mode, attributes, links in ((stat.S_IFDIR, 0x400, 1), (stat.S_IFREG, 0, 2)):
            info = SimpleNamespace(st_mode=mode, st_file_attributes=attributes, st_nlink=links)
            with patch.object(core.ctypes, 'WinDLL', return_value=kernel), \
                    patch.object(core.Path, 'lstat', return_value=info):
                with self.assertRaises(ValueError):
                    core.direct(self.profile)

    def test_system_tool_exception_is_bound_to_os_reported_powershell(self):
        kernel = MagicMock()
        def system_directory(output, _size):
            output.value = r'C:\Windows\System32'
            return len(output.value)
        kernel.GetSystemDirectoryW.side_effect = system_directory
        with patch.object(core.ctypes, 'WinDLL', return_value=kernel), \
                patch.object(core, 'direct', side_effect=lambda path, **_kw: path) as direct:
            self.assertEqual(core.native_shell(), Path(r'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'))
            direct.assert_called_once_with(Path(r'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'),
                                           missing=False, _system_tool=True)

    def test_pinned_open_checks_actual_handle_identity_before_traversal(self):
        for actual, attributes, links in ((str(self.profile.parent / 'other'), 0x10, 1),
                                          (str(self.profile), 0x410, 1),
                                          (str(self.profile), 0, 2)):
            kernel = MagicMock()
            kernel.CreateFileW.return_value = 123
            def final_name(_handle, output, _size, _flags):
                output.value = '\\\\?\\' + actual
                return len(output.value)
            def file_info(_handle, output):
                output._obj.attributes = attributes
                output._obj.links = links
                return 1
            kernel.GetFinalPathNameByHandleW.side_effect = final_name
            kernel.GetFileInformationByHandle.side_effect = file_info
            with patch.object(core.ctypes, 'WinDLL', return_value=kernel), \
                    patch.object(core, 'direct', side_effect=lambda path, **_kw: Path(path)):
                tree = core.PinnedTree(self.profile, delete=True)
                with self.assertRaises(ValueError):
                    tree.pin(self.profile, True)
                kernel.CloseHandle.assert_called_once_with(123)
                kernel.SetFileInformationByHandle.assert_not_called()

    def test_process_ownership_requires_all_exact_identity_fields(self):
        self.assertTrue(core.owned_client(self.process, self.marker, self.exe, self.profile))
        for field, value in (('pid', 78), ('created', 123457), ('exe', r'C:\another\dist\relay.exe')):
            changed = dict(self.process, **{field: value})
            with self.subTest(field=field):
                self.assertFalse(core.owned_client(changed, self.marker, self.exe, self.profile))
        for index, value in ((2, str(self.profile.parent / 'another')), (5, 'c' * 64)):
            changed = copy.deepcopy(self.process)
            changed['arguments'][index] = value
            self.assertFalse(core.owned_client(changed, self.marker, self.exe, self.profile))
        changed = copy.deepcopy(self.process)
        changed['arguments'] += ['--profile-dir', str(self.profile)]
        self.assertFalse(core.owned_client(changed, self.marker, self.exe, self.profile))

    def test_cim_command_line_is_not_adopted_from_a_recycled_pid(self):
        row = {'ProcessId': 77, 'ExecutablePath': str(self.exe), 'CommandLine': 'synthetic-owned-command',
               'CreatedFileTime': 123450}
        live = {'pid': 77, 'created': 123456, 'exe': str(self.exe)}
        self.assertTrue(core.same_process_generation(row, live))
        for changed in (dict(row, CreatedFileTime=123440), dict(row, CreatedFileTime=None),
                        dict(row, CreatedFileTime='123450'), dict(row, ProcessId=78),
                        dict(row, ExecutablePath=r'C:\another\dist\relay.exe')):
            with self.subTest(row=changed):
                self.assertFalse(core.same_process_generation(changed, live))
        reply = SimpleNamespace(returncode=0, stdout=json.dumps([row]))
        for created in (123466, 123456):
            with self.subTest(native_created=created), \
                    patch.object(core, 'native_shell', return_value=Path(r'C:\synthetic\powershell.exe')), \
                    patch.object(core, 'hidden_run', return_value=reply), \
                    patch.object(core, 'native_process', return_value=dict(live, created=created)), \
                    patch.object(core, 'split_command', return_value=self.process['arguments']) as split:
                if created == 123456:
                    processes = core.relay_processes()
                    self.assertEqual(processes[0]['arguments'], self.process['arguments'])
                    split.assert_called_once_with('synthetic-owned-command')
                else:
                    with self.assertRaises(RuntimeError):
                        core.relay_processes()
                    split.assert_not_called()

    def test_creation_precision_matches_this_headless_test_process(self):
        # Actual metadata for this controller only, no Relay/window lifecycle.
        query = ("$ErrorActionPreference='Stop'; Import-Module (Join-Path $PSHOME 'Modules\\CimCmdlets\\CimCmdlets.psd1') -Force; "
                 f"Get-CimInstance Win32_Process -Filter 'ProcessId={os.getpid()}' | "
                 "Select-Object ProcessId,ExecutablePath,@{Name='CreatedFileTime';Expression={$_.CreationDate.ToUniversalTime().ToFileTimeUtc()}} | ConvertTo-Json -Compress")
        result = core.hidden_run([str(core.native_shell()), '-NoLogo', '-NoProfile', '-NonInteractive',
                                  '-Command', query], timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        row = json.loads(result.stdout)
        self.assertTrue(core.same_process_generation(row, core.native_process(os.getpid())))

    def test_profile_reference_detects_all_occurrences_and_server_children(self):
        for arguments in (['--profile-dir', str(self.profile)],
                          ['--data=' + str(self.profile / 'server-data')],
                          ['--state-dir', str(self.profile / 'private-network')],
                          ['--profile-dir', r'C:\unrelated', '--profile-dir', str(self.profile)]):
            self.assertTrue(core.references_profile(arguments, self.profile))
        # A proven ordinary unrelated path may be ignored. Real missing or
        # redirected metadata is uncertain and must block lifecycle actions.
        with patch.object(core, 'direct', side_effect=lambda path, **_kw: path):
            self.assertFalse(core.references_profile(['--data', str(self.profile) + '-other'], self.profile))
        self.assertIsNone(core.option_value(['--profile-dir=' + str(self.profile), 'x'], '--profile-dir'))

    def test_process_aliases_cannot_hide_profile_or_owned_children(self):
        root = str(self.profile)
        aliases = (root.replace('\\', '/'), root + '\\', root + '\\..\\review-1',
                   root + r'\child\..', '\\\\?\\' + root,
                   '\\\\?\\' + root + r'\server-data',
                   '\\\\?\\' + root + r'\private-network',
                   ('\\\\?\\' + root + r'\private-network').replace('\\', '/'))
        for value in aliases:
            with self.subTest(value=value), patch.object(core, 'direct') as walk:
                self.assertTrue(core.references_profile(['relay.exe', '--profile-dir', value], self.profile))
                self.assertTrue(core.references_profile(['relay-server.exe', '--data=' + value], self.profile))
                self.assertTrue(core.references_profile(['relay-network.exe', '--state-dir', value], self.profile))
                walk.assert_not_called()
        with self.assertRaises(ValueError):
            core.local_absolute(root.replace('\\', '/'))  # Helper inputs remain strict.

    def test_uncertain_process_paths_block_without_reading_prohibited_storage(self):
        for value in (r'E:\profile', r'\\host\share\profile', r'\\?\UNC\host\profile',
                      r'\\?\Volume{unknown}\profile', r'C:\RELAY-~1\review-1',
                      r'C:\profile.', r'C:relative', '', 'C:\\' + 'a' * 32768):
            with self.subTest(value=value[:80]), patch.object(core, 'direct') as walk:
                self.assertTrue(core.references_profile(['--profile-dir', value], self.profile))
                walk.assert_not_called()
        with patch.object(core, 'direct', side_effect=ValueError('Redirected metadata')):
            self.assertTrue(core.references_profile(['--data', r'C:\ordinary-alias'], self.profile))
        with patch.object(core, 'direct', side_effect=FileNotFoundError('Unresolved metadata')):
            self.assertTrue(core.references_profile(['--state-dir', r'D:\missing-alias'], self.profile))
        with patch.object(core, 'direct') as walk:
            self.assertTrue(core.references_profile(['--profile-dir', r'\\?\C:\possible-junction\..\other'], self.profile))
            walk.assert_not_called()
        self.assertTrue(core.references_profile(['--profile-dir'], self.profile))

    def test_cua_redirect_metadata_rejects_prohibited_or_ambiguous_targets(self):
        link = Path(r'C:\checked-parent\cua-bin')
        for target in (r'E:\tool', r'\\server\share\tool', r'\\?\UNC\server\share',
                       r'\\?\Volume{unknown}\tool', r'\??\C:\tool', r'\root-relative',
                       r'C:drive-relative', r'..\relative-parent', r'C:\tool.',
                       r'C:\PROGRA~1\tool', r'C:\NUL\tool', r'cua:stream', ''):
            with self.subTest(target=target), patch.object(core.Path, 'lstat') as metadata:
                with self.assertRaises(ValueError):
                    core.cua_link_target(link, target)
                metadata.assert_not_called()
        self.assertEqual(core.cua_link_target(link, r'\\?\C:\checked\release'),
                         Path(r'C:\checked\release'))

    def test_lifecycle_diagnostics_preserve_failure_details_and_redact_owner_nonce(self):
        nonce = 'b' * 64
        result = SimpleNamespace(returncode=2, stdout='receipt ' + nonce,
                                 stderr='PermissionError: sharing violation; nonce=' + nonce)
        diagnostic = lifecycle_diagnostics('launch', result)
        self.assertIn('receipt', diagnostic)
        self.assertIn('PermissionError: sharing violation', diagnostic)
        self.assertIn('exit=2', diagnostic)
        self.assertNotIn(nonce, diagnostic)

    def test_secondary_launch_has_only_real_nonactivating_schema_fields(self):
        request = core.launch_request(self.exe, self.profile, 'b' * 64, 'unit-session',
                                      {'x': -1920, 'y': 0, 'width': 1920, 'height': 1040})
        self.assertEqual(set(request), {'path', 'additional_arguments', 'session', 'start_minimized'})
        self.assertFalse(request['start_minimized'])
        args = request['additional_arguments']
        self.assertIn('--background', args)
        self.assertIn('--quit-on-close', args)
        self.assertEqual(core.option_value(args, '--profile-dir'), str(self.profile))
        self.assertEqual(core.option_value(args, '--position'), '-1900,20')
        self.assertEqual(core.option_value(args, '--size'), '1000x1000')
        self.assertTrue(core.launch_request(self.exe, self.profile, 'b' * 64, 'unit-session', None)['start_minimized'])
        with self.assertRaises(ValueError):
            core.launch_request(self.exe, self.profile, 'b' * 64, 'unit-session',
                                {'x': -900, 'y': 0, 'width': 800, 'height': 600})

    def test_close_control_requires_an_unambiguous_fresh_full_exit_route(self):
        modern = {'label': 'Close window', 'element_token': 'snapshot:7', 'enabled': True}
        legacy = {'label': 'Close Relay', 'element_token': 'snapshot:8', 'enabled': True}
        isolated = dict(self.process, arguments=self.process['arguments'] + ['--quit-on-close'])
        self.assertEqual(core.close_control({'elements': [modern]}, isolated), modern)
        self.assertEqual(core.close_control({'elements': [legacy]}, self.process), legacy)
        for snapshot, process in (
            ({'elements': [modern]}, self.process),
            ({'elements': [modern, legacy]}, isolated),
            ({'elements': [dict(modern, enabled=False)]}, isolated),
            ({'elements': [dict(modern, element_token='')]}, isolated),
            ({'tree_markdown': '[7] Close window'}, isolated),
            ({'elements': [modern]}, dict(isolated, arguments=isolated['arguments'] + ['--quit-on-close'])),
        ):
            with self.subTest(snapshot=snapshot), self.assertRaises(RuntimeError):
                core.close_control(snapshot, process)

    def test_exited_process_object_is_not_mistaken_for_a_live_inaccessible_image(self):
        kernel = MagicMock()
        kernel.OpenProcess.return_value = 123
        kernel.WaitForSingleObject.return_value = 0
        with patch.object(core.ctypes, 'WinDLL', return_value=kernel):
            self.assertIsNone(core.native_process(77))
        kernel.QueryFullProcessImageNameW.assert_not_called()
        kernel.CloseHandle.assert_called_once_with(123)

    def test_process_exit_during_image_query_is_checked_without_adopting_another_pid(self):
        kernel = MagicMock()
        kernel.OpenProcess.return_value = 123
        kernel.WaitForSingleObject.side_effect = [0x102, 0]
        kernel.GetProcessTimes.return_value = True
        kernel.QueryFullProcessImageNameW.return_value = False
        with patch.object(core.ctypes, 'WinDLL', return_value=kernel):
            self.assertIsNone(core.native_process(77))
        kernel.OpenProcess.assert_called_once()
        kernel.CloseHandle.assert_called_once_with(123)

    def test_unknown_process_wait_status_refuses_instead_of_claiming_exit(self):
        kernel = MagicMock()
        kernel.OpenProcess.return_value = 123
        kernel.WaitForSingleObject.return_value = 0xffffffff
        with patch.object(core.ctypes, 'WinDLL', return_value=kernel), self.assertRaises(RuntimeError):
            core.native_process(77)
        kernel.QueryFullProcessImageNameW.assert_not_called()
        kernel.CloseHandle.assert_called_once_with(123)

    def test_reset_pending_or_live_profile_does_not_open_delete_handles(self):
        for marker, processes in ((dict(self.marker, launch_pending=True), []),
                                  (self.marker, [self.process])):
            with patch.object(core, 'read_json', return_value=self.manager), \
                    patch.object(core, 'PinnedTree') as tree:
                with self.assertRaises(RuntimeError):
                    core.reset_profile(self.base, self.profile, 'review-1', marker, processes, self.temp)
                tree.assert_not_called()

    def test_reset_stale_server_record_does_not_delete(self):
        with patch.object(core, 'read_json', return_value=self.manager), \
                patch.object(core, 'direct') as direct, patch.object(core, 'PinnedTree') as tree:
            direct.return_value.exists.return_value = True
            with self.assertRaises(RuntimeError):
                core.reset_profile(self.base, self.profile, 'review-1', self.marker, [], self.temp)
            tree.assert_not_called()

    def test_reset_rechecks_owner_through_pinned_handle_before_any_delete(self):
        with patch.object(core, 'read_json', return_value=self.manager) as read, \
                patch.object(core, 'direct') as direct, patch.object(core, 'PinnedTree') as tree:
            direct.return_value.exists.return_value = False
            pinned = tree.return_value.__enter__.return_value
            pinned.owner_record.return_value = dict(self.marker, name='changed')
            with self.assertRaises(RuntimeError):
                core.reset_profile(self.base, self.profile, 'review-1', self.marker, [], self.temp)
            pinned.owner_record.assert_called_once_with()
            pinned.remove.assert_not_called()
            read.assert_called_once_with(self.base / core.MANAGER)

    def test_stop_reused_pid_or_missing_client_grant_cannot_dispatch_actions(self):
        for marker, process in ((self.marker, dict(self.process, created=98765)),
                                (dict(self.marker, client=None), self.process)):
            with patch.object(core, 'relay_processes', return_value=[process]), \
                    patch.object(core, 'native_process', return_value=process), \
                    patch.object(core, 'hidden_run') as run, patch.object(core, 'driver_call') as driver, \
                    patch.object(core, 'PinnedTree') as tree:
                with self.assertRaises(RuntimeError):
                    core.stop_profile(self.profile, marker, self.exe, Path(r'C:\driver.exe'), Path(r'C:\project'))
                run.assert_not_called()
                driver.assert_not_called()
                tree.assert_not_called()

    def test_small_display_failure_precedes_pending_marker_and_launch(self):
        with patch.object(core, 'relay_processes', return_value=[]), \
                patch.object(core, 'secondary_screen', return_value={'x': 0, 'y': 0, 'width': 700, 'height': 500}), \
                patch.object(core, 'write_json') as write, patch.object(core, 'driver_call') as driver:
            with self.assertRaises(ValueError):
                core.start_profile(self.base, self.profile, self.marker, self.exe, Path(r'C:\driver.exe'))
            write.assert_not_called()
            driver.assert_not_called()

    def test_source_has_no_broad_kill_or_recursive_path_delete(self):
        source = (HERE / 'dev_profile_core.py').read_text(encoding='utf-8')
        ast.parse(source)
        for pattern in (r'(?i)taskkill', r'(?i)Stop-Process', r'(?i)TerminateProcess',
                        r'(?i)shutil\.rmtree', r'(?i)Remove-Item', r'(?i)SetForegroundWindow'):
            self.assertIsNone(re.search(pattern, source), pattern)
        self.assertIn('SetFileInformationByHandle', source)
        self.assertIn('GetFinalPathNameByHandleW', source)
        self.assertIn("'delivery_mode': 'background'", source)
        wrapper = (HERE / 'dev-profile.ps1').read_text(encoding='utf-8')
        self.assertNotIn('Get-Command', wrapper)
        self.assertNotRegex(wrapper, r'(?im)^\s*Start-Process\s')
        self.assertNotIn('Test-Path', wrapper)
        self.assertIn('$components = @($driveRoot)', wrapper)
        self.assertIn('foreach ($component in $components)', wrapper)
        self.assertIn('Get-Item -LiteralPath $component', wrapper)
        self.assertLess(wrapper.index('$components = @($driveRoot)'),
                        wrapper.index('Get-Item -LiteralPath $component'))

    def test_bootstrap_guard_stops_at_simulated_ancestor_without_leaf_probe(self):
        # Execute only the guard AST with a virtual metadata provider. This is
        # a headless PowerShell unit test, not a native Relay/Cua acceptance run.
        wrapper = str(HERE / 'dev-profile.ps1').replace("'", "''")
        script = """$ErrorActionPreference = 'Stop'
$tokens = $null; $errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile('""" + wrapper + """', [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw 'Wrapper syntax is invalid.' }
$loops = @($ast.FindAll({ param($node)
    $node -is [System.Management.Automation.Language.ForEachStatementAst] -and
    $node.Variable.VariablePath.UserPath -eq 'toolPath'
}, $true))
if ($loops.Count -ne 1) { throw 'Expected one bootstrap tool guard.' }
$text = $loops[0].Body.Extent.Text
$guard = [scriptblock]::Create($text.Substring(1, $text.Length - 2))
$script:visited = [System.Collections.Generic.List[string]]::new()
$root = [IO.Path]::GetPathRoot([Environment]::SystemDirectory)
$script:barrier = Join-Path $root 'r03-virtual-junction'
function Get-Item {
    param([string]$LiteralPath, [switch]$Force, [object]$ErrorAction)
    [void]$script:visited.Add($LiteralPath)
    if ($LiteralPath -eq $script:barrier) {
        return [pscustomobject]@{Attributes=([IO.FileAttributes]::Directory -bor [IO.FileAttributes]::ReparsePoint); PSIsContainer=$true}
    }
    if ($LiteralPath -ne $root) { throw 'A descendant of the simulated redirect was probed.' }
    return [pscustomobject]@{Attributes=[IO.FileAttributes]::Directory; PSIsContainer=$true}
}
$toolPath = Join-Path $script:barrier 'must-not-probe\\python.exe'
$refused = $false
try { & $guard } catch {
    if (-not $_.Exception.Message.Contains('Redirected tool paths are refused.')) { throw }
    $refused = $true
}
if (-not $refused -or $script:visited.Count -ne 2 -or $script:visited[0] -ne $root -or $script:visited[1] -ne $script:barrier) {
    throw 'Bootstrap did not stop root-first at the redirect metadata.'
}
'BOOTSTRAP-ROOT-FIRST-PASSED'
"""
        encoded = base64.b64encode(script.encode('utf-16-le')).decode('ascii')
        result = core.hidden_run([str(core.native_shell()), '-NoLogo', '-NoProfile', '-NonInteractive',
                                  '-EncodedCommand', encoded], timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('BOOTSTRAP-ROOT-FIRST-PASSED', result.stdout)


@unittest.skipUnless(os.name == 'nt', 'Actual local Windows filesystem checks.')
class NativeFilesystemTests(unittest.TestCase):
    """Own temporary C:/D: files only; no GUI, executable or server launch."""
    def setUp(self):
        parent = core.direct(HERE.parent, missing=False)
        self.root = core.direct(tempfile.mkdtemp(prefix='r03-native-unit-', dir=str(parent)), missing=False)
        self.directories = [self.root]
        self.files = []
        self.links = []

    def tearDown(self):
        # Remove only individually recorded objects from this fresh fixture.
        # No recursive cleanup and no traversal through junction destinations.
        for link, target in reversed(self.links):
            info = link.lstat()
            self.assertTrue(info.st_file_attributes & 0x400)
            self.assertEqual(core.cua_link_target(link, os.readlink(link)), target)
            os.rmdir(link)  # Removes this exact directory junction, not its target.
        for path in reversed(self.files):
            try:
                core.direct(path, missing=False).unlink()
            except FileNotFoundError:
                pass  # DELETE_ON_CLOSE already removed the lock this test owned.
        for directory in reversed(self.directories):
            core.direct(directory, missing=False).rmdir()

    def directory(self, relative):
        path = core.direct(self.root / relative)
        path.mkdir()
        self.directories.append(path)
        return path

    def file(self, relative, content=b'owned test fixture, never executed'):
        path = core.direct(self.root / relative)
        with path.open('xb') as handle:
            handle.write(content)
        self.files.append(path)
        return path

    def junction(self, relative, target):
        import _winapi
        path = core.direct(self.root / relative)
        core.require_local_volume(core.local_absolute(target))
        _winapi.CreateJunction(str(target), str(path))
        self.links.append((path, target))
        return path

    def test_actual_windows_lock_contention_cleanup_and_reacquisition(self):
        base = self.directory('locks')
        lock = base / '.review.controller.lock'
        self.files.append(lock)
        with core.profile_lock(base, 'review'):
            self.assertTrue(lock.exists())
            with self.assertRaises(OSError):
                with core.profile_lock(base, 'review'):
                    self.fail('A second controller acquired an already-owned lock.')
        self.assertFalse(lock.exists())
        with core.profile_lock(base, 'review'):
            self.assertTrue(lock.exists())
        self.assertFalse(lock.exists())

    def test_actual_windows_lock_exception_cleanup(self):
        base = self.directory('locks')
        lock = base / '.review.controller.lock'
        self.files.append(lock)
        with self.assertRaisesRegex(RuntimeError, 'fixture body failure'):
            with core.profile_lock(base, 'review'):
                raise RuntimeError('fixture body failure')
        self.assertFalse(lock.exists())

    def test_actual_windows_existing_lock_is_preserved(self):
        base = self.directory('locks')
        lock = self.file('locks/.review.controller.lock', b'existing owner: preserve this')
        with self.assertRaises(OSError):
            with core.profile_lock(base, 'review'):
                self.fail('An existing lock was adopted.')
        self.assertEqual(lock.read_bytes(), b'existing owner: preserve this')

    def test_actual_local_junction_chain_resolves_only_cua_tool(self):
        release = self.directory('release')
        executable = self.file('release/cua-driver.exe')
        packages = self.directory('packages')
        current = self.junction('packages/current', release)
        binary = self.junction('bin', current)
        self.assertEqual(core.resolve_cua_tool(binary / 'cua-driver.exe'), executable)
        with self.assertRaises(ValueError):
            core.resolve_cua_tool(binary / 'cua-driver.exe', max_hops=1)
        with self.assertRaises(ValueError):
            core.direct(binary / 'cua-driver.exe', missing=False)

    def test_actual_junction_metadata_cycle_is_bounded_without_following(self):
        release = self.directory('release')
        self.file('release/cua-driver.exe')
        first = self.junction('first', release)
        second = self.junction('second', first)
        readlink = os.readlink
        # Simulate first changing its metadata to second. Both inspected nodes
        # are real, local junctions; no uncertain destination is dereferenced.
        def cyclic_target(path):
            return str(second) if Path(path) == first else readlink(path)
        with patch.object(core.os, 'readlink', side_effect=cyclic_target):
            with self.assertRaisesRegex(ValueError, 'cycle'):
                core.resolve_cua_tool(first / 'cua-driver.exe')


def lifecycle_diagnostics(phase, result):
    def redact(value):
        if isinstance(value, bytes):
            value = value.decode('utf-8', errors='replace')
        value = re.sub(r'(?i)\b[0-9a-f]{64}\b', '<redacted-id>', str(value or ''))
        value = re.sub(r'(?i)(\b(?:token|password|api[_-]?key|secret|nonce)\b["\s:=]+)[^\s,"}\]]+',
                       r'\1<redacted>', value)
        return value[-8000:]
    return (f'{phase} exit={result.returncode}\nstdout:\n{redact(result.stdout)}'
            f'\nstderr:\n{redact(result.stderr)}')


def real_lifecycle(exe, driver):
    """No mocks: explicit later acceptance, bounded and profile-scoped only."""
    if os.name != 'nt':
        raise RuntimeError('Native lifecycle acceptance requires Windows and Cua Driver.')
    exe = core.direct(exe, missing=False)
    project = core.direct(HERE.parent, missing=False)
    stop_script = core.direct(project / 'scripts' / 'stop-server.ps1', missing=False)
    wrapper = core.direct(HERE / 'dev-profile.ps1', missing=False)
    shell = core.native_shell()
    name = 'r03-' + secrets.token_hex(8)
    temp = core.direct(os.environ.get('TEMP') or os.environ.get('TMP') or '', missing=False)
    base, profile = core.profile_location(temp, name)
    if profile.exists():
        raise RuntimeError('Fresh lifecycle profile unexpectedly exists.')
    common = [str(shell), '-NoLogo', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
              '-File', str(wrapper), '-Name', name, '-Exe', str(exe), '-PythonExe', sys.executable]
    if driver:
        common += ['-CuaDriver', str(core.resolve_cua_tool(driver))]
    phase, result = 'launch', None
    try:
        launched = core.hidden_run(common, timeout=45)
        result = launched
        if launched.returncode:
            raise RuntimeError('Real launch was not confirmed; retain the named profile for inspection: ' + str(profile))
        receipt = json.loads(launched.stdout)
        marker = core.read_json(profile / core.MARKER)
        process = next((p for p in core.relay_processes() if p['pid'] == receipt['pid']), None)
        if not process or not core.owned_client(process, marker, exe, profile) or not receipt.get('window'):
            raise RuntimeError('A real matching process and window were not confirmed.')
        display = core.secondary_screen()
        window = receipt['window']
        if receipt.get('launch_active') is True:
            raise RuntimeError('Cua reported the newly launched app active; background acceptance failed.')
        if display:
            bounds = window.get('bounds') or {}
            if (window.get('minimized') or not bounds
                    or bounds['x'] < display['x'] or bounds['y'] < display['y']
                    or bounds['x'] + bounds['width'] > display['x'] + display['width']
                    or bounds['y'] + bounds['height'] > display['y'] + display['height']):
                raise RuntimeError('The owned normal window is not wholly on the secondary display.')
        elif not window.get('minimized'):
            raise RuntimeError('Without a secondary display, the owned window must start minimized.')
        phase = 'stop'
        result = None
        stopped = core.hidden_run(common + ['-Stop'], timeout=60)
        result = stopped
        if stopped.returncode or any(core.references_profile(p['arguments'], profile) for p in core.relay_processes()):
            raise RuntimeError('Real graceful stop was not confirmed; retain the profile: ' + str(profile))
        stopped_receipt = json.loads(stopped.stdout)
        if not stopped_receipt.get('session_ended') or stopped_receipt.get('owned_windows_remaining') != 0:
            raise RuntimeError('Exact zero-window and ended-session shutdown was not confirmed.')
        phase = 'reset'
        result = None
        reset = core.hidden_run(common + ['-Reset'], timeout=45)
        result = reset
        if reset.returncode or profile.exists():
            raise RuntimeError('Real owned reset was not confirmed; retain the profile: ' + str(profile))
        print(json.dumps({'acceptance': 'real-native-lifecycle', 'profile': str(profile),
                          'owned_pid': receipt['pid'], 'window_confirmed': True,
                          'window': window, 'secondary_display': display,
                          'launch_active': receipt.get('launch_active'),
                          'session': marker.get('session'), 'session_ended': True,
                          'owned_windows_remaining': 0, 'owned_profile_processes_remaining': 0,
                          'stopped_without_force': True, 'reset_owned_profile_only': True}))
    except BaseException as error:
        print('Acceptance stopped. Inspect only this isolated profile: ' + str(profile), file=sys.stderr)
        if isinstance(error, subprocess.TimeoutExpired):
            result = SimpleNamespace(returncode='timeout', stdout=error.stdout, stderr=error.stderr)
        if result is not None:
            print(lifecycle_diagnostics(phase, result), file=sys.stderr)
        raise


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--lifecycle', action='store_true')
    parser.add_argument('--exe')
    parser.add_argument('--driver')
    arguments, remaining = parser.parse_known_args()
    if arguments.lifecycle:
        if not arguments.exe or remaining:
            parser.error('--lifecycle requires --exe and no unittest arguments')
        real_lifecycle(arguments.exe, arguments.driver)
    elif arguments.exe or arguments.driver:
        parser.error('--exe/--driver apply only to the explicit --lifecycle mode')
    else:
        unittest.main(argv=[sys.argv[0], *remaining])
