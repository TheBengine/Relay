"""Exercise staged update integrity and path rejection without real server/user data."""
import hashlib,json,os,pathlib,shutil,subprocess,tempfile,zipfile
root=pathlib.Path(__file__).resolve().parents[1]
workspace=root.parents[1] if root.parent.name=='outputs' else root
work=workspace/'work';work.mkdir(exist_ok=True)
shell=shutil.which('pwsh') or shutil.which('powershell')
with tempfile.TemporaryDirectory(prefix='relay-update-test-',dir=work) as tmp:
 base=pathlib.Path(tmp);destination=base/'installed with spaces';destination.mkdir();(destination/'README.md').write_text('old install')
 fake_home=base/'local-profile';fake_home.mkdir();env=os.environ.copy();env['LOCALAPPDATA']=str(fake_home)
 payload={'README.md':b'updated native app'}
 binaries=[]
 for name in ['relay.exe','relay-server.exe','livekit-server.exe']:
  data=('fixture '+name).encode();payload['dist/'+name]=data;binaries.append({'name':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()})
 payload['dist/manifest.json']=json.dumps({'version':'0.4.0','platform':'windows-x64','binaries':binaries}).encode()
 def make(name,files):
  archive=base/name
  with zipfile.ZipFile(archive,'w') as z:
   for key,data in files.items():z.writestr('Relay/'+key,data)
  return archive,hashlib.sha256(archive.read_bytes()).hexdigest()
 def run(archive,sha,install=destination,data_root=None):
  return subprocess.run([shell,'-NoProfile','-ExecutionPolicy','Bypass','-File',str(root/'scripts/update.ps1'),'-Archive',str(archive),'-ExpectedSha256',sha,'-DestinationDirectory',str(install),'-DataDirectory',str(data_root or base/'server-data')],env=env,capture_output=True,text=True,timeout=40)
 archive,sha=make('good.zip',payload)
 assert run(archive,'0'*64).returncode!=0
 assert (destination/'README.md').read_text()=='old install'
 malformed_version=json.loads(payload['dist/manifest.json']);malformed_version['version']='../0.4.0'
 bad_version,_=make('bad-version.zip',{**payload,'dist/manifest.json':json.dumps(malformed_version).encode()})
 assert run(bad_version,hashlib.sha256(bad_version.read_bytes()).hexdigest()).returncode!=0
 assert (destination/'README.md').read_text()=='old install'
 traversal,_=make('unsafe.zip',{**payload,'../escaped.txt':b'unsafe'})
 assert run(traversal,hashlib.sha256(traversal.read_bytes()).hexdigest()).returncode!=0
 assert not (base/'escaped.txt').exists()
 mismatch,_=make('bad-manifest.zip',{**payload,'dist/relay.exe':b'tampered'})
 assert run(mismatch,hashlib.sha256(mismatch.read_bytes()).hexdigest()).returncode!=0
 assert (destination/'README.md').read_text()=='old install'
 extra,_=make('unverified.zip',{**payload,'dist/extra.exe':b'unverified'})
 assert run(extra,hashlib.sha256(extra.read_bytes()).hexdigest()).returncode!=0
 # Existing junctions must be rejected before stopping a host or changing any
 # installation files. All targets and sentinels are confined to this fixture.
 outside=base/'outside installation';outside.mkdir();sentinel=outside/'example.txt';sentinel.write_text('preserve outside data')
 quote=lambda value:"'"+str(value).replace("'","''")+"'"
 def junction(link,target):
  subprocess.run([shell,'-NoProfile','-Command',f"New-Item -ItemType Junction -Path {quote(link)} -Target {quote(target)} | Out-Null"],env=env,check=True,capture_output=True)
 junction(destination/'docs',outside)
 linked_archive,linked_sha=make('junction.zip',{**payload,'docs/example.txt':b'do not redirect this update'})
 linked=run(linked_archive,linked_sha)
 assert linked.returncode!=0 and 'junction or symbolic link' in linked.stdout+linked.stderr,linked.stdout+linked.stderr
 assert sentinel.read_text()=='preserve outside data'
 assert (destination/'README.md').read_text()=='old install'
 (destination/'docs').rmdir() # Remove the fixture link itself, never its target.
 root_alias=base/'installation alias';junction(root_alias,destination)
 linked=run(archive,sha,install=root_alias)
 assert linked.returncode!=0 and 'junction or symbolic link' in linked.stdout+linked.stderr
 assert (destination/'README.md').read_text()=='old install'
 root_alias.rmdir()
 data_alias=base/'data alias';junction(data_alias,outside)
 linked=run(archive,sha,data_root=data_alias)
 assert linked.returncode!=0 and 'junction or symbolic link' in linked.stdout+linked.stderr
 assert sentinel.read_text()=='preserve outside data'
 data_alias.rmdir()
 # A stopped Relay may still have its owned SFU cleanup pending. Never overwrite
 # binaries while that managed stack record exists, even without a Relay token file.
 data_root=base/'server-data';data_root.mkdir();stack_record=data_root/'stack-instance.json';stack_record.write_text('{}',encoding='utf-8')
 pending=run(archive,sha);assert pending.returncode!=0
 assert (destination/'README.md').read_text()=='old install'
 stack_record.unlink()
 result=run(archive,sha);assert result.returncode==0,result.stdout+result.stderr
 assert (destination/'README.md').read_bytes()==payload['README.md']
 for name,data in payload.items():assert (destination/name).read_bytes()==data
 assert list(fake_home.glob('Relay/update-work/*/rollback/README.md'))[0].read_text()=='old install'
 print('PASS: checksum/manifest/traversal/junction/unverified-executable/pending-stack rejection preserve install and outside sentinels; verified update copies binaries and retains rollback. All fixture data stayed under work.')
