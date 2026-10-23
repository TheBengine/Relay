"""Capture the actual native UI against an owned, encrypted localhost fixture."""
from __future__ import annotations
import argparse, base64, hashlib, importlib.util, json, os, pathlib, socket, struct, subprocess, time, tomllib, uuid, zlib

package=pathlib.Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--client',type=pathlib.Path,default=package/'dist/relay.exe')
parser.add_argument('--server',type=pathlib.Path,default=package/'dist/relay-server.exe')
parser.add_argument('--test-binary',type=pathlib.Path,required=True)
parser.add_argument('--version',default=tomllib.loads((package/'Cargo.toml').read_text())['workspace']['package']['version'])
parser.add_argument('--only',help='Comma-separated capture cases for a follow-up review')
parser.add_argument('--variants-only',action='store_true',help='Review only non-default scale/theme variants')
parser.add_argument('--output-dir',type=pathlib.Path,help='Optional separate review directory; historical release captures remain untouched')
args=parser.parse_args()
version=args.version
capture_dir=(args.output_dir or package/'docs').resolve()
capture_dir.mkdir(parents=True,exist_ok=True)
assert all(part.isdigit() for part in version.split('.')) and len(version.split('.'))==3
workspace=package.parents[1] if package.parent.name=='outputs' else package
work=pathlib.Path(os.environ.get('RELAY_TEST_WORK_ROOT',str(workspace/'work'))).resolve()
work.mkdir(parents=True,exist_ok=True)
fixture=work/('native-qa-settings-'+version+'-'+uuid.uuid4().hex)
fixture.mkdir(parents=True)
crypto=fixture/'crypto';crypto.mkdir()
spec=importlib.util.spec_from_file_location('relay_integration',package/'scripts/test-integration.py')
helpers=importlib.util.module_from_spec(spec);spec.loader.exec_module(helpers)
port=helpers.unused_port(socket.SOCK_STREAM)
server,client,test=map(lambda p:p.resolve(),(args.server,args.client,args.test_binary))
assert os.name=='nt' and all(p.is_file() for p in (server,client,test))
password='owned-local-ui-fixture-password'
startup=subprocess.STARTUPINFO();startup.dwFlags|=subprocess.STARTF_USESHOWWINDOW;startup.wShowWindow=0
log=(fixture/'commands.log').open('w',encoding='utf-8')
process=None
record={'version':version,'scope':'Actual native screenshots; owned localhost accounts and generated data. Stream tiles are generated static fixture pixels, not physical capture. No UI-input automation.','personal_devices_opened':False,'screenshots':[],
        'client_sha256':hashlib.sha256(client.read_bytes()).hexdigest(),'server_sha256':hashlib.sha256(server.read_bytes()).hexdigest()}
try:
    process=subprocess.Popen([str(server),'--bind','127.0.0.1','--port',str(port),'--udp-port','0','--data',str(fixture/'data'),'--name','The Commons'],stdout=log,stderr=log,startupinfo=startup,creationflags=subprocess.CREATE_NO_WINDOW)
    api=helpers.API(port)
    for _ in range(100):
        try: api.call('GET','/health');break
        except OSError: time.sleep(.1)
    else: raise AssertionError('Owned server did not start')
    users=[]
    for name in ('Alex','Mira','Jonas'):
        body={'username':name,'password':password}
        if users: body['invite']=api.call('POST','/api/invites',users[0]['token'])['code']
        users.append(api.call('POST','/api/register',payload=body))
    admin=users[0]['token']
    bootstrap=api.call('GET','/api/bootstrap',admin)
    channels={'general':next(c['id'] for c in bootstrap['channels'] if c['name']=='general')}
    voice=next(c['id'] for c in bootstrap['channels'] if c['kind']=='voice')
    voice_name=next(c['name'] for c in bootstrap['channels'] if c['id']==voice)
    for key,name in [('planning','planning'),('slow','slow-chat'),('fast','fast-chat')]:
        channels[key]=api.call('POST','/api/channels',admin,{'name':name,'kind':'text','topic':'Plans for our next game night'})['id']
    api.call('PATCH','/api/server-settings',admin,{'name':'The Commons','description':'A place for friends, game nights and good conversation. Hosted on our own PC.','accent':0x5865f2,'welcome_channel_id':channels['general'],'system_channel_id':channels['planning']})
    for index,user in enumerate(users):
        api.call('PUT','/api/profile',user['token'],{'display_name':user['user']['username'],'bio':'Gaming, building things, and hanging out with friends.','appearance':{'pronouns':'they/them' if index==0 else '','accent':0x5865f2,'banner':[0x245a83,0x654a89,0x486544][index],'effect':['sparkle','wave','glow'][index]}})
        api.call('PUT','/api/presence',user['token'],{'status':['online','away','dnd'][index],'text':['Building our community','Game night at eight','Finishing a match'][index]})
    for user in users[1:]:
        api.call('POST',f"/api/friends/{user['user']['id']}",admin,{})
        api.call('POST',f"/api/friends/{users[0]['user']['id']}",user['token'],{})
    role=api.call('POST','/api/roles',admin,{'name':'Game Hosts','color':0x44bba4,'position':10,'permissions':15})
    api.call('PUT',f"/api/members/{users[1]['user']['id']}/roles",admin,{'user_id':users[1]['user']['id'],'role_ids':[role['id']]})
    group=api.call('POST','/api/conversations',admin,{'name':'Game Night Squad','kind':'group','member_ids':[u['user']['id'] for u in users[1:]]})
    channels['group']=group['channel_id']
    forum=api.call('POST','/api/conversations',admin,{'name':'game-ideas','kind':'forum'})
    thread=api.call('POST','/api/conversations',users[1]['token'],{'name':'Friday co-op plans','kind':'thread','parent_id':forum['channel_id']})
    channels['thread']=thread['channel_id']
    announcement=api.call('POST','/api/conversations',admin,{'name':'announcements','kind':'announcement'})
    channels['announcement']=announcement['channel_id']
    stage=api.call('POST','/api/conversations',admin,{'name':'Community Stage','kind':'stage'})
    channels['poll']=api.call('POST','/api/channels',admin,{'name':'game-night-vote','kind':'text'})['id']
    channels['voice_text']=next(c['id'] for c in bootstrap['channels'] if c.get('voice_parent_id')==voice)
    api.call('POST','/api/scheduled-events',admin,{'name':'Friday Game Night','description':'Bring your usual squad. We will choose a map together.','channel_id':voice,'starts_at':int(time.time())+86400,'ends_at':int(time.time())+90000})
    def chunk(kind,data): return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data)&0xffffffff)
    width,height=640,256
    pixels=b''.join(b'\0'+bytes(v for x in range(width) for v in (int(38+75*x/width),int(57+78*y/height),int(110+70*(1-x/width)))) for y in range(height))
    png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',width,height,8,2,0,0,0))+chunk(b'IDAT',zlib.compress(pixels))+chunk(b'IEND',b'')
    image_fixture=fixture/'generated-preview.png';image_fixture.write_bytes(png)
    encoded=base64.b64encode(png).decode()
    for kind,name,token in [('server_icon','community',admin),('server_banner','community',admin),('emoji','game_night',admin),('sticker','ready_to_play',admin),* [('profile_banner',u['user']['id'],u['token']) for u in users]]:
        api.call('PUT',f'/api/assets/{kind}/{name}',token,{'image_base64':encoded,'crop':{'x':0,'y':0,'width':1,'height':1}})
    legacy=api.call('POST','/api/messages',admin,{'channel_id':channels['general'],'content':'Historical compatibility example (owned preview data).','client_id':uuid.uuid4().hex})
    manifest=fixture/'private-seed-manifest.json'
    manifest.write_text(json.dumps({'base':api.base,'server_id':bootstrap['server_id'],'work_root':str(fixture),'crypto_dir':str(crypto),'users':users,'channels':channels,'gif_path':None,'legacy_message_id':legacy['id'],'mode':'initial'}),encoding='utf-8')
    env=os.environ.copy();env.update(RELAY_GUI_CRYPTO_SEED_MANIFEST=str(manifest),RELAY_TEST_WORK_ROOT=str(work))
    assert subprocess.run([str(test),'actual_server_gui_crypto_seed','--ignored','--nocapture','--test-threads=1'],env=env,cwd=package,stdout=log,stderr=log,timeout=240).returncode==0
    seed=json.loads((fixture/'crypto-seed-initial.json').read_text())
    assert seed['mutually_verified'] and seed['new_messages_opaque'] and not seed['physical_devices_opened']
    record['encrypted_fixture']=seed
    cases=[('profiles',['--settings','profiles']),('server',['--settings','server']),('channels',['--settings','server','--community-tab','channels']),('text',['--channel','general']),('voice',['--channel',voice_name]),('profiles-bottom',['--settings','profiles','--settings-bottom']),('server-bottom',['--settings','server','--settings-bottom'])]
    cases += [('encryption',['--settings','encryption']),('encryption-bottom',['--settings','encryption','--settings-bottom']),('latency',['--settings','audio'])]
    cases += [('voice-relay-hint',['--capture-panel','voice-relay-hint'])]
    cases += [('automatic-latency',['--settings','audio','--capture-panel','automatic-latency'])]
    cases += [(name,['--settings','server','--community-tab',name]) for name in ('roles','members','audit','reports','expressions','invites','backups')]
    cases += [(name,['--settings',name]) for name in ('account','privacy','connections','appearance','accessibility','notifications','keybinds','advanced')]
    cases += [('video-settings',['--settings','video']),('debug-settings',['--settings','debug'])]
    cases += [(name,['--capture-panel',name+'-home']) for name in ('friends','events','inbox')]
    cases += [('group-dm',['--channel','Game Night Squad']),('forum',['--channel','game-ideas']),('thread',['--channel','Friday co-op plans']),('announcement',['--channel','announcements']),('poll',['--channel','game-night-vote']),('stage',['--channel','Community Stage'])]
    cases += [('direct-message',['--direct','Mira']),('login',[])]
    # Connection screens: display-only join states (nothing measured or sent) and the host's Connect step.
    cases += [('join-routing',['--capture-panel','join-routing']),('join-refused',['--capture-panel','join-refused']),('share-connect',['--channel','general','--capture-panel','share-connect']),('share-change',['--channel','general','--capture-panel','share-change']),('share-change-iroh',['--channel','general','--capture-panel','share-change-iroh']),('share-firewall',['--channel','general','--capture-panel','share-firewall']),('sharing-status-off',['--channel','general','--capture-panel','sharing-status-off']),('sharing-status-on',['--channel','general','--capture-panel','sharing-status-on']),('sound-check',['--channel',voice_name,'--capture-panel','sound-check'])]
    formatted='**Game night** is on! *Bring your squad.* ~~Old plan~~\n`code stays literal` and ||a hidden surprise||\n[Community guide](https://discord.com/safety)\n```rust\nlet ready = true;\n```'
    cases += [('formatted-text',['--channel','general','--capture-send',formatted])]
    cases += [(name,['--channel',voice_name if name.startswith('stream') or name=='voice-text' else 'general','--capture-panel',name]) for name in ('voice-text','stream','stream-focus','stream-fullscreen','crop','emoji','profile-card','report','server-menu')]
    cases += [('group-settings',['--channel','Game Night Squad','--capture-panel','conversation']),('create-group',['--capture-panel','group']),('create-poll',['--capture-panel','poll'])]
    cases += [(name,['--channel','general','--capture-panel',name]) for name in ('quick-switch','create-channel','connect-community','transfers','gif-library','pins','invite','delete-message','own-profile','share-picker','camera-picker','call-icons','call-active')]
    cases += [('channels-bottom',['--settings','server','--community-tab','channels','--settings-bottom'])]
    cases += [(name,['--channel','planning' if name=='system-notices' else 'general','--capture-panel',name]) for name in ('welcome','system-notices','community-search')]
    if args.only: cases=[case for case in cases if case[0] in args.only.split(',')]
    assert cases
    last_capture_started=0.0
    # Presence fixtures are logical HTTP membership only; no physical devices.
    for user in users[1:]: api.call('POST','/api/voice/join',user['token'],{'channel_id':voice})
    api.call('POST','/api/voice/state',users[2]['token'],{'muted':True,'deafened':False,'sharing':False,'camera':False},expected=204)
    for label,options in cases:
        if label=='voice' or label=='voice-text' or label.startswith('stream'):
            for user in users[1:]: api.call('POST','/api/voice/join',user['token'],{'channel_id':voice})
            api.call('POST','/api/voice/state',users[2]['token'],{'muted':True,'deafened':False,'sharing':False,'camera':False},expected=204)
        dimensions=[('1320x850',1.0,'dark'),('940x650',1.0,'dark')]
        if label=='text': dimensions += [('1320x850',1.0,'ash'),('1320x850',1.0,'onyx')]
        if label in ('roles','voice-text','group-dm','poll','crop','thread','welcome','system-notices','community-search','profiles-bottom','encryption','encryption-bottom','latency','automatic-latency','video-settings','debug-settings'):
            dimensions += [('1320x850',1.4,'dark')]
        if label in ('friends','events','poll','expressions','welcome','system-notices','community-search','profiles-bottom','encryption','encryption-bottom','latency','automatic-latency','video-settings','debug-settings'):
            dimensions += [('940x650',1.0,'light')]
        if args.variants_only: dimensions=[entry for entry in dimensions if entry[1]!=1.0 or entry[2]!='dark']
        for size,scale,theme in dimensions:
            # Keep the display-only call state consistent with bootstrap polling.
            # This creates logical HTTP membership only; the client capture path
            # never starts a voice transport, microphone, camera or capture device.
            if label in ('call-active','sound-check'):
                for user in users:
                    api.call('POST','/api/voice/join',user['token'],{'channel_id':voice})
                api.call('POST','/api/voice/state',users[0]['token'],{'muted':True,'deafened':True,'sharing':False,'camera':False},expected=204)
            suffix='' if scale==1.0 and theme=='dark' else f'-{theme}-{int(scale*100)}pct'
            name=f'discord-layout-{version}-{label}-{size}{suffix}.png'
            image=capture_dir/name
            assert not image.exists(), 'Use a fresh capture filename or deliberately archive the previous review first'
            config=fixture/(label+'-'+size+suffix);config.mkdir()
            (config/'preferences.json').write_text(json.dumps({'server':api.base,'username':'Alex','ui_scale':scale,'appearance':{'font_scale':scale,'theme':theme}}),encoding='utf-8')
            env=os.environ.copy();env.update(RELAY_CONFIG_DIR=str(config),RELAY_CRYPTO_DIR=str(crypto),RELAY_TEST_PASSWORD=password,RELAY_CAPTURE_IMAGE=str(image_fixture))
            signed_out=label=='login' or label.startswith('join-')
            if signed_out: env.pop('RELAY_TEST_PASSWORD',None)
            # The production auth limiter permits 15 attempts/minute per IP.
            # Pace intentional captures instead of weakening or bypassing it.
            time.sleep(max(0.0,5.0-(time.monotonic()-last_capture_started)))
            last_capture_started=time.monotonic()
            for user in users[1:]: api.call('GET','/api/bootstrap',user['token'])
            auth_options=[] if signed_out else ['--capture-authenticated']
            child=subprocess.Popen([str(client),'--server',api.base,'--username','Alex','--size',size,'--capture',str(image),*auth_options,'--reduced-motion','--theme',theme,*options],env=env,cwd=package,stdout=log,stderr=log,startupinfo=startup)
            try:
                deadline=time.monotonic()+20
                while (not image.exists() or image.stat().st_size<=1000) and child.poll() is None and time.monotonic()<deadline: time.sleep(.1)
                assert image.is_file() and image.stat().st_size>1000, 'Screenshot failed; inspect owned commands.log'
                time.sleep(.5)
                record['screenshots'].append({'file':name,'sha256':hashlib.sha256(image.read_bytes()).hexdigest(),'size':size,'ui_scale':scale,'font_scale':scale,'theme':theme,'visual_review':'pending'})
                print('Captured',name,flush=True)
            finally:
                if child.poll() is None: child.terminate()
                child.wait(timeout=10)
        if label in ('call-active','sound-check'):
            api.call('POST','/api/voice/leave',users[0]['token'],expected=204)
    assert api.call('GET','/api/bootstrap',admin)['server_id']==bootstrap['server_id']
    if any(label=='formatted-text' for label,_ in cases):
        stored=api.call('GET','/api/messages?channel_id='+channels['general'],admin)
        serialized=json.dumps(stored)
        assert '**Game night**' not in serialized and 'a hidden surprise' not in serialized
        assert any(message.get('content','').startswith('relay-e2ee:v1:') for message in stored)
        record['formatted_message_transport']='New formatted bodies stay opaque in the server API; visual rendering checked separately.'
    (fixture/'visual-review.json').write_text(json.dumps(record,indent=2)+'\n',encoding='utf-8')
    if not args.only and not args.variants_only: (capture_dir/f'visual-review-{version}.json').write_text(json.dumps(record,indent=2)+'\n',encoding='utf-8')
    print('PASS:',len(record['screenshots']),'authenticated native captures and mutually verified encrypted chat fixture; visual review is separate. Evidence:',fixture,flush=True)
finally:
    if process and process.poll() is None:
        process.terminate();process.wait(timeout=10)
    log.close()
