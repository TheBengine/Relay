"""Repeated connected-idle comparison of exact owned native release processes."""
from __future__ import annotations
import argparse,csv,ctypes,datetime as dt,hashlib,importlib.util,json,os,platform,shutil,socket,statistics,subprocess,time,tomllib,zipfile
from pathlib import Path
from ctypes import wintypes

PACKAGE=Path(__file__).resolve().parent.parent
ROOT=PACKAGE.parents[1] if PACKAGE.parent.name=='outputs' else PACKAGE
spec=importlib.util.spec_from_file_location('owned_metrics',PACKAGE/'scripts/benchmark_helpers.py')
metrics_module=importlib.util.module_from_spec(spec);spec.loader.exec_module(metrics_module)

def digest(path):return metrics_module.sha256(path)
def idle_window(pid):
    """Disable input only for this test child's exact native window; keep rendering visible."""
    user=ctypes.WinDLL('user32',use_last_error=True)
    callback_type=ctypes.WINFUNCTYPE(wintypes.BOOL,wintypes.HWND,wintypes.LPARAM)
    user.EnumWindows.argtypes=[callback_type,wintypes.LPARAM];user.EnumWindows.restype=wintypes.BOOL
    user.GetWindowThreadProcessId.argtypes=[wintypes.HWND,ctypes.POINTER(wintypes.DWORD)]
    user.GetWindowTextW.argtypes=[wintypes.HWND,wintypes.LPWSTR,ctypes.c_int]
    user.EnableWindow.argtypes=[wintypes.HWND,wintypes.BOOL];user.EnableWindow.restype=wintypes.BOOL
    user.IsWindowEnabled.argtypes=[wintypes.HWND];user.IsWindowEnabled.restype=wintypes.BOOL
    user.IsWindowVisible.argtypes=[wintypes.HWND];user.IsWindowVisible.restype=wintypes.BOOL
    found=[]
    @callback_type
    def visit(hwnd,_):
        candidate=wintypes.DWORD();user.GetWindowThreadProcessId(hwnd,ctypes.byref(candidate))
        if candidate.value==pid:
            title=ctypes.create_unicode_buffer(512);user.GetWindowTextW(hwnd,title,len(title))
            if title.value=='Relay':
                user.EnableWindow(hwnd,False)
                assert not user.IsWindowEnabled(hwnd)
                found.append({'title':title.value,'visible':bool(user.IsWindowVisible(hwnd)),'input_enabled':False})
                return False
        return True
    user.EnumWindows(visit,0)
    return found[0] if found else None
def assert_idle(base,seed,config,name):
    bootstrap=metrics_module.api(base,'GET','/api/bootstrap',token=seed['alex_token'])
    assert not bootstrap['voice_members'],'Intended idle fixture joined voice'
    preferences=json.loads((config/'preferences.json').read_text(encoding='utf-8-sig'))
    assert preferences['reduced_motion'] is True
    if name=='current':
        scopes=preferences['conversations']['scopes']
        assert len(scopes)==1 and scopes[0]['current']=={'Channel':seed['channel_id']},'Intended idle conversation changed'
def main():
    if os.name != 'nt':raise SystemExit('This benchmark uses Windows process APIs.')
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--client',type=Path,default=PACKAGE/'dist/relay.exe');parser.add_argument('--server',type=Path,default=PACKAGE/'dist/relay-server.exe')
    parser.add_argument('--baseline',type=Path,required=True,help='Trusted source-included baseline release ZIP')
    parser.add_argument('--current-version',help='Defaults to workspace.package.version')
    parser.add_argument('--settle',type=int,default=45);parser.add_argument('--seconds',type=int,default=15);parser.add_argument('--repeats',type=int,default=5)
    args=parser.parse_args()
    if args.current_version is None:
        with (PACKAGE/'Cargo.toml').open('rb') as source:
            args.current_version=tomllib.load(source)['workspace']['package']['version']
    if not(15<=args.settle<=90 and 10<=args.seconds<=60 and 3<=args.repeats<=5):parser.error('Require settle15..90, seconds10..60 and repeats3..5')
    args.client,args.server,args.baseline=(path.resolve() for path in (args.client,args.server,args.baseline))
    if not all(path.is_file() for path in (args.client,args.server,args.baseline)):parser.error('Client, server and baseline must exist')
    work=ROOT/'work'/f'release-benchmark-{time.time_ns()}';work.mkdir(parents=True)
    baseline=work/'baseline.exe';current=work/'current.exe';server_exe=work/'relay-server.exe'
    with zipfile.ZipFile(args.baseline) as archive:
        if archive.getinfo('Relay/dist/manifest.json').file_size>1024*1024 or archive.getinfo('Relay/dist/relay.exe').file_size>256*1024*1024:raise ValueError('Baseline artifact is too large')
        manifest=json.loads(archive.read('Relay/dist/manifest.json').decode('utf-8-sig'))
        data=archive.read('Relay/dist/relay.exe');record=next(item for item in manifest['binaries'] if item['name']=='relay.exe')
        assert len(data)==record['bytes'] and hashlib.sha256(data).hexdigest()==record['sha256'];baseline.write_bytes(data)
    shutil.copy2(args.client,current);shutil.copy2(args.server,server_exe)
    with socket.socket() as listener:listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
    base=f'http://127.0.0.1:{port}';startup=subprocess.STARTUPINFO();startup.dwFlags|=subprocess.STARTF_USESHOWWINDOW;startup.wShowWindow=subprocess.SW_HIDE
    metadata={'status':'running','started_utc':dt.datetime.now(dt.timezone.utc).isoformat(),'host':platform.platform(),'baseline_version':manifest['version'],'current_version':args.current_version,'inputs':{'baseline_archive_sha256':digest(args.baseline),'baseline_exe_sha256':digest(baseline),'current_exe_sha256':digest(current),'server_exe_sha256':digest(server_exe)},'parameters':vars(args)|{'client':str(args.client),'server':str(args.server),'baseline':str(args.baseline)},'scenario':'Fresh same localhost server; 3 accounts, 10 channel messages, 2 DMs, no attachments; 1320x850 connected idle, reduced motion. Both exact owned visible native windows have input disabled with EnableWindow(FALSE) before warmup. No voice join and reduced-motion preferences are checked before/after sampling; the current client also exposes persisted general-channel selection for validation. No capture, microphone, camera, screenshot or working-set trimming. Alternating case order across repeats.','limits':'Observed client process CPU and RAM on this PC in visible noninteractive idle; no Discord comparison, physical-device performance, WAN latency or GPU measurement.','cases':[]}
    server=None;client=None;handle=None
    print(f'Results: {work}',flush=True)
    try:
        with (work/'server.log').open('wb') as log:
            server=subprocess.Popen([str(server_exe),'--port',str(port),'--udp-port','0','--data',str(work/'data'),'--name','The Commons'],stdout=log,stderr=log,startupinfo=startup,creationflags=subprocess.CREATE_NO_WINDOW)
        for _ in range(100):
            if server.poll() is not None:raise RuntimeError('Owned benchmark server exited')
            try:metrics_module.api(base,'GET','/health');break
            except OSError:time.sleep(.1)
        else:raise RuntimeError('Owned benchmark server not ready')
        seed=metrics_module.fixture(base)
        with (work/'samples.csv').open('w',newline='',encoding='utf-8') as stream:
            writer=csv.DictWriter(stream,fieldnames=['case','repeat','age_seconds','interval_seconds','cpu_delta_seconds','working_set_mib','private_mib','cpu_percent_one_core','cpu_percent_machine']);writer.writeheader()
            for repeat in range(1,args.repeats+1):
                order=[('baseline',baseline),('current',current)]
                if repeat%2==0:order.reverse()
                for name,exe in order:
                    config=work/f'{repeat}-{name}';config.mkdir();env=os.environ.copy();env.update(RELAY_CONFIG_DIR=str(config),RELAY_TEST_PASSWORD=seed['password'])
                    before=metrics_module.session_count(work/'data/relay.sqlite3',seed['alex_id']);began=time.perf_counter()
                    with (config/'client.log').open('wb') as log:client=subprocess.Popen([str(exe),'--server',base,'--username','Alex','--size','1320x850','--reduced-motion'],env=env,stdout=log,stderr=log,startupinfo=startup)
                    handle=metrics_module.WindowsMetrics(client.pid);assert handle.image_path.resolve()==exe.resolve()
                    window=None
                    while time.perf_counter()-began<15:
                        if client.poll() is not None:raise RuntimeError(f'{name} exited before window')
                        window=idle_window(client.pid)
                        if window and window['visible']:break
                        time.sleep(.05)
                    else:raise RuntimeError(f'{name} did not create a visible owned window')
                    while time.perf_counter()-began<25:
                        if client.poll() is not None:raise RuntimeError(f'{name} exited before sign-in')
                        if metrics_module.session_count(work/'data/relay.sqlite3',seed['alex_id'])>before:break
                        time.sleep(.1)
                    else:raise RuntimeError(f'{name} did not sign in')
                    metrics_module.wait_until(began+args.settle,client,work)
                    window=idle_window(client.pid)
                    assert window and window['visible'], f'{name} must have a visible native window for the matched scenario'
                    assert_idle(base,seed,config,name)
                    last=handle.snapshot();samples=[]
                    for _ in range(args.seconds):
                        metrics_module.wait_until(time.perf_counter()+1,client,work);snapshot=handle.snapshot();elapsed=snapshot['clock']-last['clock'];cpu=100*(snapshot['cpu_seconds']-last['cpu_seconds'])/elapsed
                        row={'case':name,'repeat':repeat,'age_seconds':snapshot['clock']-began,'interval_seconds':elapsed,'cpu_delta_seconds':snapshot['cpu_seconds']-last['cpu_seconds'],'working_set_mib':snapshot['working_set_bytes']/1048576,'private_mib':snapshot['private_bytes']/1048576,'cpu_percent_one_core':cpu,'cpu_percent_machine':cpu/handle.logical_processors};writer.writerow(row);samples.append(row);last=snapshot
                    result={'case':name,'repeat':repeat,'window':window,'pid':client.pid,'logical_processors':handle.logical_processors,**{field:statistics.median(row[field] for row in samples) for field in ['working_set_mib','private_mib','cpu_percent_one_core','cpu_percent_machine']}}
                    result['sample_seconds']=sum(row['interval_seconds'] for row in samples);result['cpu_seconds']=sum(row['cpu_delta_seconds'] for row in samples);result['cpu_mean_one_core']=100*result['cpu_seconds']/result['sample_seconds'];result['cpu_mean_machine']=result['cpu_mean_one_core']/handle.logical_processors
                    assert_idle(base,seed,config,name)
                    metadata['cases'].append(result);stream.flush();handle.close();handle=None;metrics_module.stop_owned(client);client=None
                    print(json.dumps(result),flush=True);(work/'metadata.json').write_text(json.dumps(metadata,indent=2),encoding='utf-8')
        metadata['medians']={name:{field:statistics.median(case[field] for case in metadata['cases'] if case['case']==name) for field in ['working_set_mib','private_mib','cpu_percent_one_core','cpu_percent_machine']} for name in ['baseline','current']};metadata['status']='passed'
        metadata['aggregate_cpu']={}
        for name in ['baseline','current']:
            cases=[case for case in metadata['cases'] if case['case']==name];seconds=sum(case['sample_seconds'] for case in cases);cpu_seconds=sum(case['cpu_seconds'] for case in cases)
            metadata['aggregate_cpu'][name]={'sample_seconds':seconds,'cpu_seconds':cpu_seconds,'mean_percent_one_core':100*cpu_seconds/seconds,'mean_percent_machine':100*cpu_seconds/seconds/cases[0]['logical_processors']}
    except BaseException as error:
        metadata['status']='failed';metadata['error']=f'{type(error).__name__}: {error}';raise
    finally:
        if handle:handle.close()
        metrics_module.stop_owned(client);metrics_module.stop_owned(server);metadata['finished_utc']=dt.datetime.now(dt.timezone.utc).isoformat();(work/'metadata.json').write_text(json.dumps(metadata,indent=2),encoding='utf-8');print(f'Saved {metadata["status"]}: {work}',flush=True)

if __name__=='__main__':main()
