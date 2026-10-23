"""Offline legacy import/raw backup/upgrade/restore regression using private fixtures."""
from __future__ import annotations
import argparse,hashlib,pathlib,shutil,sqlite3,subprocess,uuid
package=pathlib.Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--server',type=pathlib.Path,default=package/'dist/relay-server.exe');args=parser.parse_args()
server=args.server.resolve();workspace=package.parents[1] if package.parent.name=='outputs' else package
fixture=workspace/'work'/('backup-hosting-test-'+uuid.uuid4().hex);source=fixture/'legacy source';source.mkdir(parents=True)
db=source/'relay.sqlite3'
with sqlite3.connect(db) as c:
 c.executescript("""CREATE TABLE users(id TEXT PRIMARY KEY,username TEXT NOT NULL UNIQUE COLLATE NOCASE,password_hash TEXT NOT NULL,role TEXT NOT NULL);
 CREATE TABLE sessions(token_hash TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id),expires INTEGER NOT NULL);
 CREATE TABLE invites(code_hash TEXT PRIMARY KEY,expires INTEGER NOT NULL);
 CREATE TABLE channels(id TEXT PRIMARY KEY,name TEXT NOT NULL,kind TEXT NOT NULL,topic TEXT NOT NULL);
 CREATE TABLE messages(id TEXT PRIMARY KEY,channel_id TEXT REFERENCES channels(id),recipient_id TEXT REFERENCES users(id),author_id TEXT NOT NULL REFERENCES users(id),content TEXT NOT NULL,created_at TEXT NOT NULL,created INTEGER NOT NULL);
 CREATE TABLE uploads(id TEXT PRIMARY KEY,owner_id TEXT NOT NULL REFERENCES users(id),message_id TEXT REFERENCES messages(id),name TEXT NOT NULL,mime TEXT NOT NULL,size INTEGER NOT NULL,data BLOB NOT NULL,created INTEGER NOT NULL);
 CREATE TABLE reactions(message_id TEXT NOT NULL REFERENCES messages(id),emoji TEXT NOT NULL,user_id TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(message_id,emoji,user_id));
 INSERT INTO users VALUES('fixture-user','FixtureAdmin','fixture-invalid-password-hash','admin');
 INSERT INTO channels VALUES('fixture-channel','general','text','legacy');
 INSERT INTO messages VALUES('fixture-message','fixture-channel',NULL,'fixture-user','preserved legacy content','2026-10-03T00:00:00Z',1);
 INSERT INTO uploads VALUES('fixture-upload','fixture-user','fixture-message','legacy.txt','text/plain',6,X'6c6567616379',1);
 """)
(source/'uploads').mkdir();(source/'uploads/legacy-blob.bin').write_bytes(b'legacy filesystem attachment');original=hashlib.sha256(db.read_bytes()).hexdigest()
log=open(fixture/'commands.log','w',encoding='utf-8')
def command(*parts):return subprocess.run([str(server),*map(str,parts)],stdout=log,stderr=log,timeout=20).returncode
shell=shutil.which('pwsh') or shutil.which('powershell')
def import_to(destination,confirm=True):
 cmd=[shell,'-NoProfile','-ExecutionPolicy','Bypass','-File',str(package/'scripts/import-community.ps1'),'-SourceDirectory',str(source),'-DataDirectory',str(destination),'-ServerBinary',str(server)]
 if confirm:cmd.append('-OfflineConfirmed')
 return subprocess.run(cmd,stdout=log,stderr=log,timeout=20).returncode
snapshot=fixture/'raw.sqlite3';assert command('--data',source,'--raw-backup',snapshot)==0
assert hashlib.sha256(db.read_bytes()).hexdigest()==original
assert command('--data',source,'--raw-backup',snapshot)!=0,'existing backup must never be overwritten'
destination=fixture/'imported community';assert import_to(destination,False)!=0;assert not destination.exists()
occupied=fixture/'occupied';occupied.mkdir();(occupied/'keep.txt').write_text('preserve');assert import_to(occupied)!=0;assert (occupied/'keep.txt').read_text()=='preserve'
assert import_to(destination)==0
assert (destination/'uploads/legacy-blob.bin').read_bytes()==b'legacy filesystem attachment';assert hashlib.sha256(db.read_bytes()).hexdigest()==original
with sqlite3.connect(db) as c:assert c.execute('PRAGMA user_version').fetchone()[0]==0
assert command('--data',destination,'--migrate-only')==0
with sqlite3.connect(destination/'relay.sqlite3') as c:
 assert c.execute('PRAGMA user_version').fetchone()[0]>=4
 assert c.execute('SELECT data FROM uploads').fetchone()[0]==b'legacy'
 assert c.execute('SELECT content FROM messages').fetchone()[0]=='preserved legacy content'
 assert c.execute('SELECT COUNT(*) FROM schema_migrations').fetchone()[0]>=4
assert list((destination/'backups').glob('pre-migration-*.sqlite3'))
with sqlite3.connect(destination/'relay.sqlite3') as c:
 stable_id=c.execute("SELECT value FROM metadata WHERE key='server_id'").fetchone()[0]
 stable_order=c.execute('SELECT * FROM message_order').fetchall()
 stable_migrations=c.execute('SELECT * FROM schema_migrations').fetchall()
assert command('--data',destination,'--migrate-only')==0
with sqlite3.connect(destination/'relay.sqlite3') as c:
 assert c.execute("SELECT value FROM metadata WHERE key='server_id'").fetchone()[0]==stable_id
 assert c.execute('SELECT * FROM message_order').fetchall()==stable_order
 assert c.execute('SELECT * FROM schema_migrations').fetchall()==stable_migrations
assert command('--data',destination,'--restore',snapshot)==0,'legacy restore migrates a copied source'
corrupt=fixture/'corrupt.sqlite3';corrupt.write_bytes(b'corrupt')
assert command('--data',destination,'--restore',corrupt)!=0
with sqlite3.connect(destination/'relay.sqlite3') as c:assert c.execute('SELECT content FROM messages').fetchone()[0]=='preserved legacy content';assert c.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
log.close();print('PASS: offline-required/nonempty-refused import, legacy filesystem/BLOB preservation, raw snapshot leaves source unchanged, numbered upgrade + safety backup, legacy/corrupt restore. Evidence',fixture)
