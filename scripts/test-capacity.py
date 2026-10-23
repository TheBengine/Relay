"""Measure actual MLS device/group limits in five fresh owned Windows processes."""
from __future__ import annotations
import argparse, datetime, hashlib, json, os, pathlib, re, subprocess, uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--test-binary', type=pathlib.Path, required=True, help='Explicit optimized relay test executable')
parser.add_argument('--work', type=pathlib.Path, help='New owned evidence directory; must not already exist')
args = parser.parse_args()
binary = args.test_binary.resolve()
assert os.name == 'nt' and binary.is_file(), 'Windows and an explicit compiled test binary are required'
package = pathlib.Path(__file__).resolve().parents[1]
run = (args.work or package/'work'/('crypto-capacity-'+uuid.uuid4().hex)).resolve()
run.mkdir(parents=True, exist_ok=False)
scratch = run/'scenarios'
scratch.mkdir()
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
names = ['one_device','sixteen_devices','thirty_two_devices','sixty_four_devices','one_hundred_twenty_eight_groups']
prefix = 'e2ee::capacity_tests::actual_capacity_'
inventory = subprocess.run([str(binary),'--list'], capture_output=True, text=True, encoding='utf-8', timeout=15)
assert inventory.returncode == 0 and all(prefix+name+': test' in inventory.stdout for name in names)
env = dict(os.environ, RELAY_TEST_WORK_ROOT=str(scratch))
results = []
for name in names:
    print('Measuring '+name, flush=True)
    log = run/(name+'.log')
    command = [str(binary),prefix+name,'--exact','--ignored','--nocapture','--test-threads=1']
    with log.open('w', encoding='utf-8') as output:
        result = subprocess.run(command, cwd=package, env=env, stdout=output, stderr=subprocess.STDOUT,
                                creationflags=subprocess.CREATE_NO_WINDOW, timeout=900)
    contents = log.read_text(encoding='utf-8')
    matches = re.findall(r'Capacity evidence: ([^\r\n]+)', contents)
    evidence = None
    if matches:
        assert len(matches) == 1
        path = pathlib.Path(matches[0].strip()).resolve()
        assert path.is_relative_to(scratch) and path.is_file()
        document = json.loads(path.read_text(encoding='utf-8'))
        assert document['private_keys_exported'] is False and document['physical_devices_opened'] is False
        evidence = {'path':str(path),'sha256':sha(path),'result':document['result'],'samples':document['samples'],
                    'process_id':document['process_id'],'scope':document['scope'],'memory_method':document['memory_method']}
    passed = result.returncode == 0 and 'test result: ok. 1 passed;' in contents and evidence is not None
    results.append({'case':name,'passed':passed,'returncode':result.returncode,'command':command,
                    'log':str(log),'log_sha256':sha(log),'evidence':evidence})
    (run/'progress.json').write_text(json.dumps(results,indent=2)+'\n',encoding='utf-8')
    if passed:
        print('PASS '+name,flush=True)
    else:
        print('FAIL '+name+'; evidence retained in '+str(log),flush=True)
record = {'checked_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),
          'test_binary':str(binary),'test_binary_sha256':sha(binary),'fresh_process_per_case':True,
          'cases':results,'passed':all(r['passed'] for r in results),
          'scope':'Optimized actual local protected actors. Does not accept HTTP key-stock races, server capacity, native UI, physical devices, WAN or whole-app media performance.'}
(run/'verification.json').write_text(json.dumps(record,indent=2)+'\n',encoding='utf-8')
print(json.dumps({'passed':record['passed'],'cases':len(results),'evidence':str(run/'verification.json')}),flush=True)
raise SystemExit(0 if record['passed'] else 1)
