//! Durable community state. All permission checks use current database state.
use super::*;
use axum::extract::OriginalUri;
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 27;
pub const READ: u32 = 1;
pub const SEND: u32 = 2;
pub const JOIN: u32 = 4;
pub const STREAM: u32 = 8;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Dedicated account-wide Friends service. Requires its own new database.
    pub people: bool,
    pub account_provider: Option<crate::account_identity::Provider>,
    pub storage_limit_bytes: u64,
    pub user_storage_limit_bytes: u64,
    pub trusted_proxy_ips: Vec<std::net::IpAddr>,
    /// Exact public DNS names accepted by the HTTP Host guard. IP literals are accepted directly.
    pub allowed_hosts: Vec<String>,
    pub rtc: Option<crate::rtc::RtcConfig>,
    pub local_control_token: Option<String>,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            people: false,
            account_provider: None,
            storage_limit_bytes: 10 * 1024 * 1024 * 1024,
            user_storage_limit_bytes: 1024 * 1024 * 1024,
            trusted_proxy_ips: vec![],
            allowed_hosts: vec![],
            rtc: None,
            local_control_token: None,
        }
    }
}

fn add_column(tx: &Connection, table: &str, name: &str, declaration: &str) -> rusqlite::Result<()> {
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !names.iter().any(|column| column == name) {
        tx.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {name} {declaration}"
        ))?;
    }
    Ok(())
}

pub fn migrate_database(db: &mut Connection) -> rusqlite::Result<()> {
    let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(rusqlite::Error::InvalidParameterName(
            "Database belongs to a newer Relay release".into(),
        ));
    }
    for next in version + 1..=SCHEMA_VERSION {
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        match next {
            1 => {
                tx.execute_batch("CREATE TABLE IF NOT EXISTS users(id TEXT PRIMARY KEY,username TEXT NOT NULL UNIQUE COLLATE NOCASE,password_hash TEXT NOT NULL,role TEXT NOT NULL);
                    CREATE TABLE IF NOT EXISTS sessions(token_hash TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id),expires INTEGER NOT NULL);
                    CREATE TABLE IF NOT EXISTS invites(code_hash TEXT PRIMARY KEY,expires INTEGER NOT NULL);
                    CREATE TABLE IF NOT EXISTS channels(id TEXT PRIMARY KEY,name TEXT NOT NULL,kind TEXT NOT NULL,topic TEXT NOT NULL);
                    CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY,channel_id TEXT REFERENCES channels(id),recipient_id TEXT REFERENCES users(id),author_id TEXT NOT NULL REFERENCES users(id),content TEXT NOT NULL,created_at TEXT NOT NULL,created INTEGER NOT NULL,edited_at TEXT,CHECK((channel_id IS NULL)!=(recipient_id IS NULL)));
                    CREATE TABLE IF NOT EXISTS uploads(id TEXT PRIMARY KEY,owner_id TEXT NOT NULL REFERENCES users(id),message_id TEXT REFERENCES messages(id) ON DELETE CASCADE,name TEXT NOT NULL,mime TEXT NOT NULL,size INTEGER NOT NULL,data BLOB NOT NULL,created INTEGER NOT NULL);
                    CREATE TABLE IF NOT EXISTS reactions(message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,emoji TEXT NOT NULL,user_id TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(message_id,emoji,user_id));
                    CREATE INDEX IF NOT EXISTS message_channel ON messages(channel_id,created);
                    CREATE INDEX IF NOT EXISTS message_dm ON messages(author_id,recipient_id,created);
                    CREATE INDEX IF NOT EXISTS upload_owner ON uploads(owner_id);
                    CREATE INDEX IF NOT EXISTS session_user ON sessions(user_id);
                    CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
                    CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY,applied_at INTEGER NOT NULL);")?;
                add_column(&tx,"messages","edited_at","TEXT")?;
                tx.execute("INSERT OR IGNORE INTO metadata VALUES('server_id',?1)",[id()])?;
            }
            2 => {
                add_column(&tx,"channels","visibility","TEXT NOT NULL DEFAULT 'public'")?;
                add_column(&tx,"channels","category_id","TEXT")?;
                add_column(&tx,"channels","position","INTEGER NOT NULL DEFAULT 0")?;
                add_column(&tx,"messages","reply_to_id","TEXT REFERENCES messages(id) ON DELETE SET NULL")?;
                add_column(&tx,"messages","client_id","TEXT")?;
                add_column(&tx,"messages","request_hash","TEXT")?;
                add_column(&tx,"invites","remaining_uses","INTEGER NOT NULL DEFAULT 1")?;
                add_column(&tx,"invites","label","TEXT NOT NULL DEFAULT ''")?;
                add_column(&tx,"sessions","created_at","INTEGER NOT NULL DEFAULT 0")?;
                tx.execute_batch("CREATE UNIQUE INDEX message_client_request ON messages(author_id,client_id) WHERE client_id IS NOT NULL;
                    CREATE TABLE message_order(sequence INTEGER PRIMARY KEY AUTOINCREMENT,message_id TEXT NOT NULL UNIQUE REFERENCES messages(id) ON DELETE CASCADE);
                    INSERT INTO message_order(message_id) SELECT id FROM messages ORDER BY rowid;
                    CREATE TABLE membership(user_id TEXT PRIMARY KEY REFERENCES users(id),status TEXT NOT NULL DEFAULT 'active');
                    INSERT INTO membership(user_id) SELECT id FROM users;
                    CREATE TABLE channel_permissions(channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,subject_kind TEXT NOT NULL,subject_id TEXT NOT NULL,permissions INTEGER NOT NULL,PRIMARY KEY(channel_id,subject_kind,subject_id));
                    CREATE TABLE categories(id TEXT PRIMARY KEY,name TEXT NOT NULL,position INTEGER NOT NULL DEFAULT 0);
                    CREATE TABLE mentions(message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,user_id TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(message_id,user_id));
                    CREATE TABLE read_states(user_id TEXT NOT NULL REFERENCES users(id),target_kind TEXT NOT NULL,target_id TEXT NOT NULL,last_read_sequence INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(user_id,target_kind,target_id));
                    CREATE TABLE message_changes(sequence INTEGER PRIMARY KEY AUTOINCREMENT,kind TEXT NOT NULL,message_id TEXT NOT NULL,channel_id TEXT,recipient_id TEXT,author_id TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0);
                    INSERT INTO message_changes(kind,message_id,channel_id,recipient_id,author_id) SELECT 'created',id,channel_id,recipient_id,author_id FROM messages ORDER BY rowid;")?;
            }
            3 => tx.execute_batch("CREATE VIRTUAL TABLE message_search USING fts5(message_id UNINDEXED,content);
                INSERT INTO message_search(message_id,content) SELECT id,content FROM messages;
                CREATE TRIGGER message_search_insert AFTER INSERT ON messages BEGIN INSERT INTO message_search(message_id,content) VALUES(new.id,new.content); END;
                CREATE TRIGGER message_search_update AFTER UPDATE OF content ON messages BEGIN DELETE FROM message_search WHERE message_id=old.id; INSERT INTO message_search(message_id,content) VALUES(new.id,new.content); END;
                CREATE TRIGGER message_search_delete AFTER DELETE ON messages BEGIN DELETE FROM message_search WHERE message_id=old.id; END;")?,
            4 => tx.execute_batch("CREATE TABLE send_receipts(author_id TEXT NOT NULL REFERENCES users(id),client_id TEXT NOT NULL,request_hash TEXT NOT NULL,message_id TEXT REFERENCES messages(id) ON DELETE SET NULL,PRIMARY KEY(author_id,client_id));
                INSERT INTO send_receipts SELECT author_id,client_id,request_hash,id FROM messages WHERE client_id IS NOT NULL;")?,
            5 => {
                add_column(&tx,"channels","slowmode_seconds","INTEGER NOT NULL DEFAULT 0 CHECK(slowmode_seconds BETWEEN 0 AND 21600)")?;
                tx.execute_batch("CREATE TABLE channel_pins(message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,pinned_by TEXT NOT NULL REFERENCES users(id),pinned_at TEXT NOT NULL,pinned_ms INTEGER NOT NULL);
                    CREATE INDEX channel_pins_order ON channel_pins(channel_id,pinned_ms DESC,message_id);
                    CREATE TRIGGER channel_pin_scope BEFORE INSERT ON channel_pins WHEN NOT EXISTS(SELECT 1 FROM messages WHERE id=new.message_id AND channel_id=new.channel_id) BEGIN SELECT RAISE(ABORT,'Pin must belong to channel'); END;
                    CREATE TABLE channel_send_cooldowns(user_id TEXT NOT NULL REFERENCES users(id),channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,last_sent_ms INTEGER NOT NULL CHECK(last_sent_ms>=0),PRIMARY KEY(user_id,channel_id));
                    INSERT INTO channel_send_cooldowns SELECT author_id,channel_id,MAX(created)*1000 FROM messages WHERE channel_id IS NOT NULL GROUP BY author_id,channel_id;
                    CREATE TABLE message_send_log(sequence INTEGER PRIMARY KEY AUTOINCREMENT,user_id TEXT NOT NULL REFERENCES users(id),sent_at_ms INTEGER NOT NULL CHECK(sent_at_ms>=0));
                    CREATE INDEX message_send_window ON message_send_log(user_id,sent_at_ms);
                    CREATE INDEX message_send_expiry ON message_send_log(sent_at_ms);")?;
                tx.execute("INSERT INTO message_send_log(user_id,sent_at_ms) SELECT author_id,created*1000 FROM messages WHERE created>?1",[now().saturating_sub(5)])?;
            }
            6 => tx.execute_batch("CREATE TABLE user_profiles(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,display_name TEXT NOT NULL DEFAULT '',bio TEXT NOT NULL DEFAULT '',version INTEGER NOT NULL DEFAULT 0 CHECK(version>=0),avatar BLOB);\n                INSERT INTO user_profiles(user_id) SELECT id FROM users;")?,
            7 => tx.execute_batch("CREATE TABLE crypto_devices(device_id TEXT PRIMARY KEY,user_id TEXT NOT NULL REFERENCES users(id),identity TEXT NOT NULL,key_package TEXT NOT NULL,active INTEGER NOT NULL DEFAULT 1 CHECK(active IN(0,1)));
                CREATE INDEX crypto_device_owner ON crypto_devices(user_id,active);
                CREATE TABLE crypto_key_packages(device_id TEXT NOT NULL REFERENCES crypto_devices(device_id),package_hash TEXT NOT NULL,key_package TEXT NOT NULL,target_kind TEXT,target_id TEXT,PRIMARY KEY(device_id,package_hash),CHECK((target_kind IS NULL)=(target_id IS NULL)));
                CREATE UNIQUE INDEX crypto_package_target ON crypto_key_packages(device_id,target_kind,target_id) WHERE target_kind IS NOT NULL;
                CREATE TABLE crypto_groups(target_kind TEXT NOT NULL,target_id TEXT NOT NULL,group_id TEXT NOT NULL UNIQUE,sequence INTEGER NOT NULL DEFAULT 0 CHECK(sequence>=0),epoch INTEGER NOT NULL DEFAULT 0 CHECK(epoch>=0),PRIMARY KEY(target_kind,target_id));
                CREATE TABLE crypto_events(group_id TEXT NOT NULL REFERENCES crypto_groups(group_id) ON DELETE CASCADE,sequence INTEGER NOT NULL,epoch INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN('create','commit','welcome')),author_user_id TEXT NOT NULL REFERENCES users(id),author_device_id TEXT NOT NULL REFERENCES crypto_devices(device_id),recipient_device_id TEXT REFERENCES crypto_devices(device_id),body TEXT NOT NULL,operation_id TEXT NOT NULL,PRIMARY KEY(group_id,sequence));
                CREATE TABLE crypto_operations(group_id TEXT NOT NULL REFERENCES crypto_groups(group_id) ON DELETE CASCADE,operation_id TEXT NOT NULL,author_user_id TEXT NOT NULL REFERENCES users(id),author_device_id TEXT NOT NULL REFERENCES crypto_devices(device_id),request_hash TEXT NOT NULL,PRIMARY KEY(group_id,operation_id));
                DROP TRIGGER message_search_insert;DROP TRIGGER message_search_update;
                DELETE FROM message_search WHERE substr(content,1,14)='relay-e2ee:v1:';
                CREATE TRIGGER message_search_insert AFTER INSERT ON messages WHEN substr(new.content,1,14)!='relay-e2ee:v1:' BEGIN INSERT INTO message_search(message_id,content) VALUES(new.id,new.content); END;
                CREATE TRIGGER message_search_update AFTER UPDATE OF content ON messages BEGIN DELETE FROM message_search WHERE message_id=old.id; INSERT INTO message_search(message_id,content) SELECT new.id,new.content WHERE substr(new.content,1,14)!='relay-e2ee:v1:'; END;")?,
            8 => tx.execute_batch("CREATE TABLE crypto_welcome_packages(group_id TEXT NOT NULL,sequence INTEGER NOT NULL,device_id TEXT NOT NULL,package_hash TEXT NOT NULL,PRIMARY KEY(group_id,sequence),UNIQUE(device_id,package_hash),FOREIGN KEY(group_id,sequence) REFERENCES crypto_events(group_id,sequence) ON DELETE CASCADE,FOREIGN KEY(device_id,package_hash) REFERENCES crypto_key_packages(device_id,package_hash));
                CREATE TABLE crypto_key_package_consumptions(device_id TEXT NOT NULL,package_hash TEXT NOT NULL,group_id TEXT NOT NULL,welcome_sequence INTEGER NOT NULL,target_kind TEXT NOT NULL,target_id TEXT NOT NULL,PRIMARY KEY(device_id,package_hash),FOREIGN KEY(device_id,package_hash) REFERENCES crypto_key_packages(device_id,package_hash),FOREIGN KEY(group_id,welcome_sequence) REFERENCES crypto_welcome_packages(group_id,sequence));
                INSERT OR IGNORE INTO crypto_welcome_packages(group_id,sequence,device_id,package_hash) SELECT e.group_id,e.sequence,e.recipient_device_id,p.package_hash FROM crypto_events e JOIN crypto_groups g ON g.group_id=e.group_id JOIN crypto_key_packages p ON p.device_id=e.recipient_device_id AND p.target_kind=g.target_kind AND p.target_id=g.target_id WHERE e.kind='welcome' ORDER BY e.sequence;")?,
            9 => add_column(&tx, "user_profiles", "appearance", "TEXT NOT NULL DEFAULT '{}'" )?,
            10 => tx.execute_batch("CREATE TABLE community_roles(id TEXT PRIMARY KEY,name TEXT NOT NULL,color INTEGER NOT NULL CHECK(color BETWEEN 0 AND 16777215),position INTEGER NOT NULL CHECK(position BETWEEN 1 AND 1000),permissions INTEGER NOT NULL CHECK(permissions BETWEEN 0 AND 1023));
                CREATE TABLE member_roles(user_id TEXT NOT NULL REFERENCES users(id),role_id TEXT NOT NULL REFERENCES community_roles(id) ON DELETE CASCADE,PRIMARY KEY(user_id,role_id));
                CREATE TABLE audit_log(sequence INTEGER PRIMARY KEY AUTOINCREMENT,actor_id TEXT NOT NULL REFERENCES users(id),action TEXT NOT NULL,target_id TEXT NOT NULL,created_at TEXT NOT NULL);
                CREATE INDEX member_roles_by_role ON member_roles(role_id);")?,
            11 => {
                add_column(&tx,"channels","voice_parent_id","TEXT REFERENCES channels(id)")?;
                tx.execute_batch("CREATE UNIQUE INDEX voice_text_parent ON channels(voice_parent_id) WHERE voice_parent_id IS NOT NULL;")?;
            }
            12 => tx.execute_batch("CREATE TABLE friendships(sender TEXT NOT NULL REFERENCES users(id),recipient TEXT NOT NULL REFERENCES users(id),accepted INTEGER NOT NULL DEFAULT 0 CHECK(accepted IN(0,1)),PRIMARY KEY(sender,recipient),CHECK(sender!=recipient));
                CREATE TABLE user_blocks(owner TEXT NOT NULL REFERENCES users(id),peer TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(owner,peer),CHECK(owner!=peer));
                CREATE TABLE user_presence(user_id TEXT PRIMARY KEY REFERENCES users(id),status TEXT NOT NULL DEFAULT 'online',text TEXT NOT NULL DEFAULT '',expires_at INTEGER,last_seen INTEGER NOT NULL);
                CREATE TABLE user_reports(id TEXT PRIMARY KEY,reporter_id TEXT NOT NULL REFERENCES users(id),reported_id TEXT NOT NULL REFERENCES users(id),reason TEXT NOT NULL,evidence TEXT NOT NULL DEFAULT '',created_at TEXT NOT NULL,resolved INTEGER NOT NULL DEFAULT 0 CHECK(resolved IN(0,1)));")?,
            13 => tx.execute_batch("CREATE TABLE conversations(channel_id TEXT PRIMARY KEY REFERENCES channels(id) ON DELETE CASCADE,kind TEXT NOT NULL,owner_id TEXT NOT NULL REFERENCES users(id),parent_id TEXT REFERENCES channels(id),archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN(0,1)));
                CREATE TABLE conversation_members(channel_id TEXT NOT NULL REFERENCES conversations(channel_id) ON DELETE CASCADE,user_id TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(channel_id,user_id));
                CREATE TABLE scheduled_events(id TEXT PRIMARY KEY,name TEXT NOT NULL,description TEXT NOT NULL,channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,starts_at INTEGER NOT NULL,ends_at INTEGER NOT NULL);
                CREATE TABLE event_interest(event_id TEXT NOT NULL REFERENCES scheduled_events(id) ON DELETE CASCADE,user_id TEXT NOT NULL REFERENCES users(id),PRIMARY KEY(event_id,user_id));
                CREATE TABLE stage_members(channel_id TEXT NOT NULL REFERENCES conversations(channel_id) ON DELETE CASCADE,user_id TEXT NOT NULL REFERENCES users(id),speaker INTEGER NOT NULL DEFAULT 0 CHECK(speaker IN(0,1)),raised INTEGER NOT NULL DEFAULT 0 CHECK(raised IN(0,1)),PRIMARY KEY(channel_id,user_id));")?,
            14 => tx.execute_batch("CREATE TABLE presentation_assets(id TEXT PRIMARY KEY,kind TEXT NOT NULL,name TEXT NOT NULL COLLATE NOCASE,owner_id TEXT NOT NULL REFERENCES users(id),version INTEGER NOT NULL DEFAULT 1 CHECK(version>0),data BLOB NOT NULL,UNIQUE(kind,name));")?,
            15 => tx.execute_batch("CREATE TABLE community_system_events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,user_id TEXT NOT NULL REFERENCES users(id),kind TEXT NOT NULL CHECK(kind IN('joined','left')),created_at TEXT NOT NULL);")?,
            16 => tx.execute_batch("CREATE TABLE crypto_call_requests(group_id TEXT NOT NULL REFERENCES crypto_groups(group_id) ON DELETE CASCADE,author_user_id TEXT NOT NULL REFERENCES users(id),author_device_id TEXT NOT NULL REFERENCES crypto_devices(device_id),operation_id TEXT NOT NULL,request_json TEXT NOT NULL,acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN(0,1)),PRIMARY KEY(group_id,author_device_id),UNIQUE(group_id,operation_id));")?,
            17 => tx.execute_batch("CREATE TABLE voice_resume_leases(user_id TEXT PRIMARY KEY REFERENCES users(id),token_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE,channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,device_id TEXT NOT NULL REFERENCES crypto_devices(device_id),capability_hash TEXT NOT NULL UNIQUE,instance_id TEXT NOT NULL,expires INTEGER NOT NULL,muted INTEGER NOT NULL CHECK(muted IN(0,1)),deafened INTEGER NOT NULL CHECK(deafened IN(0,1)));
                CREATE TRIGGER voice_resume_device_revoke AFTER UPDATE OF active ON crypto_devices WHEN new.active=0 BEGIN DELETE FROM voice_resume_leases WHERE device_id=new.device_id; END;
                CREATE TRIGGER voice_resume_channel_permission_insert AFTER INSERT ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=new.channel_id; END;
                CREATE TRIGGER voice_resume_channel_permission_update AFTER UPDATE ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=old.channel_id OR channel_id=new.channel_id; END;
                CREATE TRIGGER voice_resume_channel_permission_delete AFTER DELETE ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=old.channel_id; END;")?,
            // Invitation-based device verification: which invitation a member joined
            // with (a side table, so no existing `users` insert changes), and the proof
            // their device made with that invitation's secret. The server stores the
            // proof but cannot create or check it.
            18 => tx.execute_batch("CREATE TABLE IF NOT EXISTS member_invitations(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,invite_id TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS device_attestations(device_id TEXT PRIMARY KEY REFERENCES crypto_devices(device_id),user_id TEXT NOT NULL REFERENCES users(id),invite_id TEXT NOT NULL,mac TEXT NOT NULL,created INTEGER NOT NULL);")?,
            // Protected leases retain their device FK. Open leases bind to an
            // authenticated session and exact accepted mode revision instead.
            19 => tx.execute_batch("DROP TRIGGER voice_resume_device_revoke;
                DROP TRIGGER voice_resume_channel_permission_insert;
                DROP TRIGGER voice_resume_channel_permission_update;
                DROP TRIGGER voice_resume_channel_permission_delete;
                CREATE TABLE voice_resume_leases_new(user_id TEXT PRIMARY KEY REFERENCES users(id),token_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE,channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,device_id TEXT REFERENCES crypto_devices(device_id),capability_hash TEXT NOT NULL UNIQUE,instance_id TEXT NOT NULL,expires INTEGER NOT NULL,muted INTEGER NOT NULL CHECK(muted IN(0,1)),deafened INTEGER NOT NULL CHECK(deafened IN(0,1)),open_revision INTEGER CHECK(open_revision>=0),CHECK((device_id IS NOT NULL AND open_revision IS NULL) OR (device_id IS NULL AND open_revision IS NOT NULL)));
                INSERT INTO voice_resume_leases_new SELECT *,NULL FROM voice_resume_leases;
                DROP TABLE voice_resume_leases;
                ALTER TABLE voice_resume_leases_new RENAME TO voice_resume_leases;
                CREATE TRIGGER voice_resume_device_revoke AFTER UPDATE OF active ON crypto_devices WHEN new.active=0 BEGIN DELETE FROM voice_resume_leases WHERE device_id=new.device_id; END;
                CREATE TRIGGER voice_resume_channel_permission_insert AFTER INSERT ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=new.channel_id; END;
                CREATE TRIGGER voice_resume_channel_permission_update AFTER UPDATE ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=old.channel_id OR channel_id=new.channel_id; END;
                CREATE TRIGGER voice_resume_channel_permission_delete AFTER DELETE ON channel_permissions BEGIN DELETE FROM voice_resume_leases WHERE channel_id=old.channel_id; END;")?,
            20 => tx.execute_batch("CREATE TABLE authenticator_credentials(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, sealed BLOB NOT NULL, salt BLOB NOT NULL, revision TEXT NOT NULL, last_step INTEGER NOT NULL DEFAULT -1, attempts INTEGER NOT NULL DEFAULT 0, window_start INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE authenticator_setup(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, session_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE, setup_id TEXT NOT NULL, sealed BLOB NOT NULL, salt BLOB NOT NULL, expires INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, window_start INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE authenticator_recovery(user_id TEXT NOT NULL REFERENCES authenticator_credentials(user_id) ON DELETE CASCADE, code_hash TEXT NOT NULL, PRIMARY KEY(user_id,code_hash));")?,
            21 => tx.execute_batch("CREATE TABLE linked_identities(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,issuer TEXT NOT NULL,subject TEXT NOT NULL,handle TEXT NOT NULL,verified_at INTEGER NOT NULL,UNIQUE(issuer,subject),UNIQUE(issuer,handle));
                CREATE TABLE identity_link_challenges(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,session_hash TEXT NOT NULL REFERENCES sessions(token_hash) ON DELETE CASCADE,challenge_id TEXT NOT NULL UNIQUE,nonce TEXT NOT NULL,password_hash TEXT NOT NULL,expires INTEGER NOT NULL,issuer TEXT NOT NULL);")?,
            22 => tx.execute_batch("CREATE TABLE identity_login_challenges(challenge_id TEXT PRIMARY KEY,nonce TEXT NOT NULL,issuer TEXT NOT NULL,invite_hash TEXT NOT NULL,register INTEGER NOT NULL,initial_protection TEXT,expires INTEGER NOT NULL);")?,
            23 => tx.execute_batch(include_str!("private_call_recovery.sql"))?,
            // M2 slice 2: replica state, key-only change log and its triggers (off until enabled).
            24 => crate::replica::migrate(&tx)?,
            // Downgrade barrier: pre-handover hosts must refuse this database,
            // rather than ignoring the external retirement journal.
            25 => {},
            // Friends groups: waiting invitations. Private to the two people and the group.
            26 => tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS group_invitations(channel_id TEXT NOT NULL REFERENCES conversations(channel_id) ON DELETE CASCADE,invitee TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,inviter TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,created INTEGER NOT NULL,PRIMARY KEY(channel_id,invitee));
                CREATE INDEX IF NOT EXISTS group_invitations_invitee ON group_invitations(invitee);",
            )?,
            // Host locator directory (Friends only): opaque encrypted, host-signed records.
            27 => tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS host_locator_records(id TEXT PRIMARY KEY CHECK(length(id)=64),anchor TEXT NOT NULL,current_key TEXT NOT NULL,current_term INTEGER NOT NULL,issued_ms INTEGER NOT NULL,expires_ms INTEGER NOT NULL,record TEXT NOT NULL);
                CREATE INDEX IF NOT EXISTS host_locator_anchor ON host_locator_records(anchor,expires_ms);
                CREATE INDEX IF NOT EXISTS host_locator_expiry ON host_locator_records(expires_ms);",
            )?,
            _ => unreachable!(),
        }
        tx.execute(
            "INSERT INTO schema_migrations VALUES(?1,?2)",
            params![next, now()],
        )?;
        tx.pragma_update(None, "user_version", next)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn server_id(db: &Connection) -> rusqlite::Result<String> {
    db.query_row(
        "SELECT value FROM metadata WHERE key='server_id'",
        [],
        |r| r.get(0),
    )
}
pub fn active_user(db: &Connection, user_id: &str) -> ApiResult<()> {
    if db
        .query_row(
            "SELECT status FROM membership WHERE user_id=?1",
            [user_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        != Some("active")
    {
        return Err(forbidden());
    }
    Ok(())
}
pub fn channel_access(
    db: &Connection,
    channel_id: &str,
    user_id: &str,
    permission: u32,
) -> ApiResult<()> {
    active_user(db, user_id)?;
    if people::enabled(db) {
        if let Some(c) = conversations::info(db, channel_id)? {
            let group = if c.kind == "group_voice" {
                conversations::info(db, c.parent_id.as_deref().ok_or_else(forbidden)?)?
                    .ok_or_else(forbidden)?
            } else {
                c
            };
            if group.kind != "group" || !people::group_access(db, &group, user_id) {
                return Err(forbidden());
            }
        } else {
            return Err(forbidden());
        }
    }
    if let Some(result) = conversations::access(db, channel_id, user_id, permission)? {
        return result;
    }
    if let Some(parent) = db
        .query_row(
            "SELECT voice_parent_id FROM channels WHERE id=?1",
            [channel_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
    {
        // The companion is a separate text/MLS target, with the parent's current ACL.
        // It remains readable when no call is active; JOIN is not required to chat.
        return channel_access(db, &parent, user_id, permission);
    }
    let role = get_user(db, user_id)?.role;
    let visibility: String = db
        .query_row(
            "SELECT visibility FROM channels WHERE id=?1",
            [channel_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(missing)?;
    if roles::has(db, user_id, PERMISSION_ADMIN) {
        return Ok(());
    }
    let personal=db.query_row("SELECT permissions FROM channel_permissions WHERE channel_id=?1 AND subject_kind='user' AND subject_id=?2",params![channel_id,user_id],|r|r.get::<_,u32>(0)).optional()?;
    let baseline=db.query_row("SELECT permissions FROM channel_permissions WHERE channel_id=?1 AND subject_kind='role' AND subject_id=?2",params![channel_id,role],|r|r.get::<_,u32>(0)).optional()?.unwrap_or(if visibility=="public" {roles::permissions(db,user_id)? & 15} else {0});
    let mut query=db.prepare("SELECT p.permissions FROM channel_permissions p JOIN member_roles m ON m.role_id=p.subject_id WHERE p.channel_id=?1 AND p.subject_kind='role' AND m.user_id=?2")?;
    let custom = query
        .query_map(params![channel_id, user_id], |r| r.get::<_, u32>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    // Custom role overrides replace the baseline; grants win across assigned roles.
    // An explicit member override is final. All omitted bits in an entry are denied.
    let bits = personal.unwrap_or_else(|| {
        if custom.is_empty() {
            baseline
        } else {
            custom.into_iter().fold(0, |a, b| a | b)
        }
    });
    if bits & READ == 0 || bits & permission != permission {
        return Err(forbidden());
    }
    Ok(())
}
pub fn row_channel(r: &rusqlite::Row<'_>) -> rusqlite::Result<Channel> {
    Ok(Channel {
        voice_parent_id: r.get(8)?,
        id: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        topic: r.get(3)?,
        visibility: r.get(4)?,
        category_id: r.get(5)?,
        position: r.get(6)?,
        can_send: true,
        can_join: true,
        can_stream: true,
        slowmode_seconds: r.get(7)?,
        slowmode_bypass: false,
        can_manage_pins: false,
    })
}
pub fn get_channel(db: &Connection, channel_id: &str) -> ApiResult<Channel> {
    db.query_row(
        "SELECT id,name,kind,topic,visibility,category_id,position,slowmode_seconds,voice_parent_id FROM channels WHERE id=?1",
        [channel_id],
        row_channel,
    )
    .optional()?
    .ok_or_else(missing)
}
pub fn visible_channels(db: &Connection, user_id: &str) -> ApiResult<Vec<Channel>> {
    // The Friends service holds every member's groups in one database, so a
    // member is only ever asked about the conversations they belong to.
    let people = people::enabled(db);
    let sql = if people {
        format!("{} SELECT id,name,kind,topic,visibility,category_id,position,slowmode_seconds,voice_parent_id FROM channels WHERE id IN (SELECT id FROM mine) OR id IN (SELECT id FROM rooms) OR voice_parent_id IN (SELECT id FROM rooms) ORDER BY position,rowid", conversations::MINE)
    } else {
        "SELECT id,name,kind,topic,visibility,category_id,position,slowmode_seconds,voice_parent_id FROM channels ORDER BY position,rowid".into()
    };
    let mut statement = db.prepare(&sql)?;
    let channels = if people {
        statement
            .query_map([user_id], row_channel)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        statement
            .query_map([], row_channel)?
            .collect::<Result<Vec<_>, _>>()?
    };
    Ok(channels
        .into_iter()
        .filter(|c| channel_access(db, &c.id, user_id, READ).is_ok())
        .map(|mut c| {
            c.can_send = channel_access(db, &c.id, user_id, SEND).is_ok();
            c.can_join = channel_access(db, &c.id, user_id, JOIN).is_ok();
            c.can_stream = channel_access(db, &c.id, user_id, STREAM).is_ok();
            let staff = roles::has(db, user_id, PERMISSION_MESSAGES);
            c.slowmode_bypass = staff;
            c.can_manage_pins = staff && c.kind == "text";
            c
        })
        .collect())
}
pub fn record_change(db: &Connection, message: &Message, kind: &str) -> rusqlite::Result<()> {
    db.execute("INSERT INTO message_changes(kind,message_id,channel_id,recipient_id,author_id,deleted) VALUES(?1,?2,?3,?4,?5,?6)",params![kind,message.id,message.channel_id,message.recipient_id,message.author.id,kind=="deleted"])?;
    Ok(())
}
pub fn latest_change(db: &Connection) -> rusqlite::Result<u64> {
    db.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM message_changes",
        [],
        |r| r.get(0),
    )
}

#[derive(Deserialize, Default)]
pub struct HistoryQuery {
    pub channel_id: Option<String>,
    pub peer_id: Option<String>,
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub limit: Option<u32>,
    pub search: Option<String>,
    pub around: Option<String>,
}
fn target(
    db: &Connection,
    user_id: &str,
    channel: &Option<String>,
    peer: &Option<String>,
) -> ApiResult<(String, String)> {
    match (channel, peer) {
        (Some(id), None) => {
            channel_access(db, id, user_id, READ)?;
            if get_channel(db, id)?.kind != "text" {
                return Err(bad("Choose a text channel"));
            }
            Ok(("channel".into(), id.clone()))
        }
        (None, Some(id)) => {
            if people::enabled(db) && !people::visible(db, user_id, id) {
                return Err(forbidden());
            }
            get_user(db, id)?;
            Ok(("direct".into(), id.clone()))
        }
        _ => Err(bad("Choose exactly one channel_id or peer_id")),
    }
}
pub fn history_page(db: &Connection, user_id: &str, q: &HistoryQuery) -> ApiResult<HistoryPage> {
    let (kind, target) = target(db, user_id, &q.channel_id, &q.peer_id)?;
    if q.before.is_some_and(|v| v > i64::MAX as u64) || q.after.is_some_and(|v| v > i64::MAX as u64)
    {
        return Err(bad("Invalid message cursor"));
    }
    if let Some(id) = &q.around {
        if q.before.is_some() || q.after.is_some() || q.search.is_some() {
            return Err(bad(
                "Around cannot be combined with another cursor or search",
            ));
        }
        message_access(db, id, user_id)?;
        let message = get_message(db, id)?;
        if message.channel_id != q.channel_id
            || (kind == "direct"
                && !((message.author.id == user_id
                    && message.recipient_id.as_deref() == Some(target.as_str()))
                    || (message.author.id == target
                        && message.recipient_id.as_deref() == Some(user_id))))
        {
            return Err(forbidden());
        }
        let mut older = history_page(
            db,
            user_id,
            &HistoryQuery {
                channel_id: q.channel_id.clone(),
                peer_id: q.peer_id.clone(),
                before: Some(message.sequence + 1),
                limit: Some(50),
                ..Default::default()
            },
        )?;
        let newer = history_page(
            db,
            user_id,
            &HistoryQuery {
                channel_id: q.channel_id.clone(),
                peer_id: q.peer_id.clone(),
                after: Some(message.sequence),
                limit: Some(50),
                ..Default::default()
            },
        )?;
        older.messages.extend(newer.messages);
        return Ok(older);
    }
    let limit = q.limit.unwrap_or(100);
    if !(1..=200).contains(&limit)
        || (q.before.is_some() && q.after.is_some())
        || q.search.as_ref().is_some_and(|s| s.len() > 256)
    {
        return Err(bad("Invalid history cursor, search or limit"));
    }
    let query = q
        .search
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")));
    let condition = if kind == "channel" {
        "m.channel_id=?2"
    } else {
        "m.channel_id IS NULL AND ((m.author_id=?1 AND m.recipient_id=?2) OR (m.author_id=?2 AND m.recipient_id=?1))"
    };
    let search = if query.is_some() {
        " AND m.id IN(SELECT message_id FROM message_search WHERE message_search MATCH ?6)"
    } else {
        " AND ?6 IS NULL"
    };
    let latest:u64=db.query_row(&format!("SELECT COALESCE(MAX(o.sequence),0) FROM message_order o JOIN messages m ON m.id=o.message_id WHERE {condition}"),params![user_id,target],|r|r.get(0))?;
    let direction = if q.after.is_some() { "ASC" } else { "DESC" };
    let mut statement=db.prepare(&format!("SELECT m.id FROM messages m JOIN message_order o ON o.message_id=m.id WHERE {condition} AND (?3 IS NULL OR o.sequence<?3) AND (?4 IS NULL OR o.sequence>?4){search} ORDER BY o.sequence {direction} LIMIT ?5"))?;
    let ids = statement
        .query_map(
            params![user_id, target, q.before, q.after, limit + 1, query],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let more = ids.len() > limit as usize;
    let mut messages = ids
        .into_iter()
        .take(limit as usize)
        .map(|id| get_message(db, &id))
        .collect::<ApiResult<Vec<_>>>()?;
    if q.after.is_none() {
        messages.reverse();
    }
    let next_before = if more && q.after.is_none() {
        messages.first().map(|m| m.sequence)
    } else {
        None
    };
    Ok(HistoryPage {
        messages,
        next_before,
        latest_sequence: latest,
    })
}
pub async fn history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<HistoryPage>> {
    let auth = state.auth(&headers)?;
    Ok(Json(history_page(&lock(&state.0.db), &auth.user.id, &q)?))
}

fn read_state(
    db: &Connection,
    user_id: &str,
    channel: Option<String>,
    peer: Option<String>,
) -> ApiResult<ReadState> {
    let (kind, target_id) = target(db, user_id, &channel, &peer)?;
    let read=db.query_row("SELECT last_read_sequence FROM read_states WHERE user_id=?1 AND target_kind=?2 AND target_id=?3",params![user_id,kind,target_id],|r|r.get::<_,u64>(0)).optional()?.unwrap_or(0);
    let condition = if kind == "channel" {
        "m.channel_id=?2"
    } else {
        "m.channel_id IS NULL AND ((m.author_id=?1 AND m.recipient_id=?2) OR (m.author_id=?2 AND m.recipient_id=?1))"
    };
    let(unread,mentions)=db.query_row(&format!("SELECT COUNT(*),COALESCE(SUM(EXISTS(SELECT 1 FROM mentions x WHERE x.message_id=m.id AND x.user_id=?1)),0) FROM messages m JOIN message_order o ON o.message_id=m.id WHERE {condition} AND o.sequence>?3 AND m.author_id!=?1"),params![user_id,target_id,read],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(ReadState {
        channel_id: channel,
        peer_id: peer,
        last_read_sequence: read,
        unread_count: unread,
        mention_count: mentions,
    })
}
pub fn all_read_states(db: &Connection, user_id: &str) -> ApiResult<Vec<ReadState>> {
    let mut result = vec![];
    for channel in visible_channels(db, user_id)?
        .into_iter()
        .filter(|c| c.kind == "text")
    {
        result.push(read_state(db, user_id, Some(channel.id), None)?);
    }
    let mut statement=db.prepare("SELECT DISTINCT CASE WHEN author_id=?1 THEN recipient_id ELSE author_id END FROM messages WHERE channel_id IS NULL AND(author_id=?1 OR recipient_id=?1)")?;
    for peer in statement
        .query_map([user_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
    {
        if let Ok(read) = read_state(db, user_id, None, Some(peer)) {
            result.push(read);
        }
    }
    Ok(result)
}
pub async fn read_states(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ReadState>>> {
    let auth = state.auth(&headers)?;
    Ok(Json(all_read_states(&lock(&state.0.db), &auth.user.id)?))
}
pub async fn mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<MarkRead>,
) -> ApiResult<Json<ReadState>> {
    let auth = state.auth(&headers)?;
    let result = {
        let db = lock(&state.0.db);
        let (kind, target_id) = target(&db, &auth.user.id, &input.channel_id, &input.peer_id)?;
        let page = history_page(
            &db,
            &auth.user.id,
            &HistoryQuery {
                channel_id: input.channel_id.clone(),
                peer_id: input.peer_id.clone(),
                limit: Some(1),
                ..Default::default()
            },
        )?;
        let bounded = input.last_read_sequence.min(page.latest_sequence);
        db.execute("INSERT INTO read_states VALUES(?1,?2,?3,?4) ON CONFLICT(user_id,target_kind,target_id) DO UPDATE SET last_read_sequence=MAX(last_read_sequence,excluded.last_read_sequence)",params![auth.user.id,kind,target_id,bounded])?;
        read_state(&db, &auth.user.id, input.channel_id, input.peer_id)?
    };
    state.notify(
        "read-state",
        result.channel_id.clone(),
        Some(auth.user.id.clone()),
        Some(vec![auth.user.id]),
    );
    Ok(Json(result))
}
#[derive(Deserialize)]
pub struct ChangesQuery {
    pub after: Option<u64>,
    pub limit: Option<u32>,
}
pub async fn changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<ChangePage>> {
    let auth = state.auth(&headers)?;
    let limit = q.limit.unwrap_or(200);
    if !(1..=200).contains(&limit) || q.after.is_some_and(|v| v > i64::MAX as u64) {
        return Err(bad("Invalid change cursor or limit"));
    }
    let db = lock(&state.0.db);
    let mut statement=db.prepare("SELECT sequence,kind,message_id,channel_id,recipient_id,author_id,deleted FROM message_changes WHERE sequence>?1 ORDER BY sequence LIMIT ?2")?;
    let records = statement
        .query_map(params![q.after.unwrap_or(0), limit + 1], |r| {
            Ok(MessageChange {
                sequence: r.get(0)?,
                kind: r.get(1)?,
                message_id: r.get(2)?,
                channel_id: r.get(3)?,
                recipient_id: r.get(4)?,
                user_id: r.get(5)?,
                deleted: r.get(6)?,
                message: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = records.len() > limit as usize;
    let mut cursor = q.after.unwrap_or(0);
    let mut changes = vec![];
    for mut c in records.into_iter().take(limit as usize) {
        cursor = c.sequence;
        let allowed = match &c.channel_id {
            Some(channel) => channel_access(&db, channel, &auth.user.id, READ).is_ok(),
            None => {
                c.user_id == auth.user.id
                    || c.recipient_id.as_deref() == Some(auth.user.id.as_str())
            }
        };
        if allowed {
            c.message = get_message(&db, &c.message_id).ok();
            c.deleted = c.deleted || c.message.is_none();
            changes.push(c);
        }
    }
    Ok(Json(ChangePage {
        changes,
        next_cursor: cursor,
        has_more,
    }))
}

pub fn require_admin(auth: &Auth) -> ApiResult<()> {
    if auth.permissions & PERMISSION_ADMIN == 0 {
        return Err(forbidden());
    }
    Ok(())
}
pub(super) fn require_moderator(auth: &Auth) -> ApiResult<()> {
    if auth.permissions & (PERMISSION_MEMBERS | PERMISSION_ADMIN) == 0 {
        return Err(forbidden());
    }
    Ok(())
}
pub async fn channel_details(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Channel>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    channel_access(&db, &id, &auth.user.id, READ)?;
    Ok(Json(
        visible_channels(&db, &auth.user.id)?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(missing)?,
    ))
}
pub async fn edit_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<EditChannel>,
) -> ApiResult<Json<Channel>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    let result = {
        let mut connection = lock(&state.0.db);
        let db = connection.transaction()?;
        roles::require(&db, &auth.user.id, PERMISSION_CHANNELS)?;
        let mut channel = get_channel(&db, &id)?;
        conversations::require_regular(&db, &id)?;
        if channel.voice_parent_id.is_some() {
            return Err(bad("Edit the parent voice channel instead"));
        }
        if let Some(name) = input.name {
            if name.trim().is_empty() || name.len() > 64 {
                return Err(bad("Invalid channel name"));
            }
            channel.name = name.trim().into();
        }
        if let Some(topic) = input.topic {
            if topic.len() > 500 {
                return Err(bad("Invalid channel topic"));
            }
            channel.topic = topic;
        }
        if let Some(visibility) = input.visibility {
            if !matches!(visibility.as_str(), "public" | "private") {
                return Err(bad("Invalid channel visibility"));
            }
            channel.visibility = visibility;
        }
        if let Some(category) = input.category_id {
            channel.category_id = if category.is_empty() {
                None
            } else {
                if db.query_row(
                    "SELECT COUNT(*) FROM categories WHERE id=?1",
                    [&category],
                    |r| r.get::<_, u32>(0),
                )? == 0
                {
                    return Err(missing());
                }
                Some(category)
            };
        }
        if let Some(position) = input.position {
            channel.position = position;
        }
        if let Some(seconds) = input.slowmode_seconds {
            if seconds > MAX_SLOWMODE_SECONDS || (seconds > 0 && channel.kind != "text") {
                return Err(bad("Slowmode must be 0–21600 seconds on a text channel"));
            }
            channel.slowmode_seconds = seconds;
        }
        channel.slowmode_bypass = true;
        channel.can_manage_pins = channel.kind == "text";
        db.execute("UPDATE channels SET name=?2,topic=?3,visibility=?4,category_id=?5,position=?6,slowmode_seconds=?7 WHERE id=?1",params![id,channel.name,channel.topic,channel.visibility,channel.category_id,channel.position,channel.slowmode_seconds])?;
        roles::audit(&db, &auth.user.id, "channel.updated", &id)?;
        db.commit()?;
        channel
    };
    state.enforce_voice_permissions()?;
    state.notify("channels", None, None, None);
    Ok(Json(result))
}
pub async fn delete_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        if get_channel(&tx, &id)?.voice_parent_id.is_some() {
            return Err(bad("Delete the parent voice channel instead"));
        }
        conversations::require_regular(&tx, &id)?;
        conversations::delete_tree(&tx, &id)?;
        roles::audit(&tx, &auth.user.id, "channel.deleted", &id)?;
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}
pub async fn permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<SetChannelPermissions>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    let db = lock(&state.0.db);
    get_channel(&db, &id)?;
    conversations::require_regular(&db, &id)?;
    if get_channel(&db, &id)?.voice_parent_id.is_some() {
        return Err(bad("Use the parent voice channel's permissions"));
    }
    let mut s = db.prepare(
        "SELECT subject_kind,subject_id,permissions FROM channel_permissions WHERE channel_id=?1",
    )?;
    let entries = s
        .query_map([id], |r| {
            let bits: u32 = r.get(2)?;
            Ok(ChannelPermission {
                subject_kind: r.get(0)?,
                subject_id: r.get(1)?,
                read: bits & READ != 0,
                send: bits & SEND != 0,
                join: bits & JOIN != 0,
                stream: bits & STREAM != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(SetChannelPermissions { entries }))
}
pub async fn set_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<SetChannelPermissions>,
) -> ApiResult<Json<SetChannelPermissions>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    if input.entries.len() > 512 {
        return Err(bad("Too many permission entries"));
    }
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
        get_channel(&tx, &id)?;
        conversations::require_regular(&tx, &id)?;
        if get_channel(&tx, &id)?.voice_parent_id.is_some() {
            return Err(bad("Use the parent voice channel's permissions"));
        }
        tx.execute("DELETE FROM channel_permissions WHERE channel_id=?1", [&id])?;
        for e in &input.entries {
            match e.subject_kind.as_str() {
                "user" => {
                    get_user(&tx, &e.subject_id)?;
                }
                "role" => {
                    if !matches!(e.subject_id.as_str(), "admin" | "moderator" | "member")
                        && tx.query_row(
                            "SELECT COUNT(*) FROM community_roles WHERE id=?1",
                            [&e.subject_id],
                            |r| r.get::<_, u32>(0),
                        )? == 0
                    {
                        return Err(bad("Unknown role"));
                    }
                }
                _ => return Err(bad("Invalid permission subject")),
            };
            let bits = u32::from(e.read) * READ
                + u32::from(e.send) * SEND
                + u32::from(e.join) * JOIN
                + u32::from(e.stream) * STREAM;
            tx.execute(
                "INSERT INTO channel_permissions VALUES(?1,?2,?3,?4)",
                params![id, e.subject_kind, e.subject_id, bits],
            )?;
        }
        roles::audit(&tx, &auth.user.id, "channel.permissions.updated", &id)?;
        tx.commit()?;
    }
    state.enforce_voice_permissions()?;
    state.notify("channels", None, None, None);
    Ok(Json(input))
}

pub async fn list_members(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    require_moderator(&auth)?;
    let db = lock(&state.0.db);
    let mut query=db.prepare("SELECT u.id,u.username,u.role,m.status FROM users u JOIN membership m ON m.user_id=u.id ORDER BY u.username COLLATE NOCASE")?;
    let rows=query.query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"username":r.get::<_,String>(1)?,"role":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
    Ok(Json(serde_json::json!(rows)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberEdit {
    pub role: Option<String>,
    pub status: Option<String>,
}
pub async fn edit_member(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<MemberEdit>,
) -> ApiResult<Json<User>> {
    let auth = state.auth(&headers)?;
    require_moderator(&auth)?;
    let result = {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let target = get_user(&tx, &id)?;
        roles::require(&tx, &auth.user.id, PERMISSION_MEMBERS)?;
        if auth.user.role != "admin" {
            roles::require_below(&tx, &auth.user.id, &id)?;
        }
        if auth.user.role != "admin" && (target.role != "member" || input.role.is_some()) {
            return Err(forbidden());
        }
        let role = input.role.unwrap_or(target.role.clone());
        if !matches!(role.as_str(), "admin" | "moderator" | "member") {
            return Err(bad("Invalid role"));
        }
        let current_status: String = tx.query_row(
            "SELECT status FROM membership WHERE user_id=?1",
            [&id],
            |r| r.get(0),
        )?;
        let status = input.status.unwrap_or(current_status.clone());
        if !matches!(status.as_str(), "active" | "banned" | "removed") {
            return Err(bad("Invalid member status"));
        }
        if target.role=="admin" && (role!="admin"||status!="active") && tx.query_row("SELECT COUNT(*) FROM users u JOIN membership m ON m.user_id=u.id WHERE role='admin' AND status='active'",[],|r|r.get::<_,u32>(0))?<=1{return Err(bad("Keep at least one active administrator"));}
        tx.execute("UPDATE users SET role=?2 WHERE id=?1", params![id, role])?;
        tx.execute(
            "UPDATE membership SET status=?2 WHERE user_id=?1",
            params![id, status],
        )?;
        if (current_status == "active") != (status == "active") {
            crate::server_settings::record_system_event(
                &tx,
                &id,
                if status == "active" { "joined" } else { "left" },
            )?;
        }
        tx.execute("DELETE FROM sessions WHERE user_id=?1", [&id])?;
        roles::audit(&tx, &auth.user.id, "member.updated", &id)?;
        tx.commit()?;
        get_user(&db, &id)?
    };
    state.remove_voice(&lock(&state.0.db), &id, None)?;
    state.notify("members", None, Some(id), None);
    Ok(Json(result))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CreateInvite {
    pub max_uses: Option<u32>,
    pub expires_in: Option<u64>,
    pub label: Option<String>,
}
pub async fn create_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_INVITES)?;
    let input: CreateInvite = if bytes.is_empty() || bytes.as_ref() == b"null" {
        Default::default()
    } else {
        serde_json::from_slice(&bytes).map_err(|_| bad("Invalid invite options"))?
    };
    let uses = input.max_uses.unwrap_or(1);
    let expiry = input.expires_in.unwrap_or(86400);
    let label = input.label.unwrap_or_default();
    if !(1..=1000).contains(&uses) || !(60..=30 * 86400).contains(&expiry) || label.len() > 100 {
        return Err(bad("Invalid invite policy"));
    }
    let mut connection = lock(&state.0.db);
    let db = connection.transaction()?;
    roles::require(&db, &auth.user.id, PERMISSION_INVITES)?;
    let code = random_token();
    let hash = token_hash(&code);
    let expires = now() + expiry;
    db.execute(
        "INSERT INTO invites(code_hash,expires,remaining_uses,label) VALUES(?1,?2,?3,?4)",
        params![hash, expires, uses, label],
    )?;
    roles::audit(&db, &auth.user.id, "invite.created", &hash)?;
    db.commit()?;
    Ok(Json(
        serde_json::json!({"id":hash,"code":code,"expires_in":expiry,"expires_at":expires,"remaining_uses":uses,"label":label}),
    ))
}
pub async fn list_invites(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_INVITES)?;
    let db = lock(&state.0.db);
    let mut s=db.prepare("SELECT code_hash,expires,remaining_uses,label FROM invites WHERE expires>?1 AND remaining_uses>0 ORDER BY expires")?;
    let rows=s.query_map([now()],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"expires_at":r.get::<_,u64>(1)?,"remaining_uses":r.get::<_,u32>(2)?,"label":r.get::<_,String>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
    Ok(Json(serde_json::json!(rows)))
}
pub async fn revoke_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_INVITES)?;
    let mut connection = lock(&state.0.db);
    let db = connection.transaction()?;
    roles::require(&db, &auth.user.id, PERMISSION_INVITES)?;
    db.execute("DELETE FROM invites WHERE code_hash=?1", [&id])?;
    roles::audit(&db, &auth.user.id, "invite.revoked", &id)?;
    db.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn categories(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let visible = visible_channels(&db, &auth.user.id)?
        .into_iter()
        .filter_map(|c| c.category_id)
        .collect::<Vec<_>>();
    let mut statement =
        db.prepare("SELECT id,name,position FROM categories ORDER BY position,rowid")?;
    let rows=statement.query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"position":r.get::<_,i64>(2)?})))?.collect::<Result<Vec<_>,_>>()?.into_iter().filter(|category|roles::has(&db,&auth.user.id,PERMISSION_CHANNELS)||category["id"].as_str().is_some_and(|id|visible.iter().any(|v|v==id))).collect::<Vec<_>>();
    Ok(Json(serde_json::json!(rows)))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryInput {
    pub name: String,
    #[serde(default)]
    pub position: i64,
}
pub async fn create_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CategoryInput>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    if input.name.trim().is_empty() || input.name.len() > 64 {
        return Err(bad("Invalid category name"));
    }
    let id = id();
    let mut connection = lock(&state.0.db);
    let tx = connection.transaction()?;
    roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
    tx.execute(
        "INSERT INTO categories VALUES(?1,?2,?3)",
        params![id, input.name.trim(), input.position],
    )?;
    roles::audit(&tx, &auth.user.id, "category.created", &id)?;
    tx.commit()?;
    drop(connection);
    state.notify("channels", None, None, None);
    Ok(Json(
        serde_json::json!({"id":id,"name":input.name.trim(),"position":input.position}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryEdit {
    pub name: Option<String>,
    pub position: Option<i64>,
}
pub async fn edit_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<CategoryEdit>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    if input
        .name
        .as_ref()
        .is_some_and(|name| name.trim().is_empty() || name.len() > 64)
    {
        return Err(bad("Invalid category name"));
    }
    let result = {
        let mut connection = lock(&state.0.db);
        let db = connection.transaction()?;
        roles::require(&db, &auth.user.id, PERMISSION_CHANNELS)?;
        let existing: Option<(String, i64)> = db
            .query_row(
                "SELECT name,position FROM categories WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (name, position) = existing.ok_or_else(missing)?;
        let name = input
            .name
            .map(|name| name.trim().to_owned())
            .unwrap_or(name);
        let position = input.position.unwrap_or(position);
        db.execute(
            "UPDATE categories SET name=?2,position=?3 WHERE id=?1",
            params![id, name, position],
        )?;
        roles::audit(&db, &auth.user.id, "category.updated", &id)?;
        db.commit()?;
        serde_json::json!({"id":id,"name":name,"position":position})
    };
    state.notify("channels", None, None, None);
    Ok(Json(result))
}
pub async fn delete_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    roles::require(&lock(&state.0.db), &auth.user.id, PERMISSION_CHANNELS)?;
    let mut db = lock(&state.0.db);
    let tx = db.transaction()?;
    roles::require(&tx, &auth.user.id, PERMISSION_CHANNELS)?;
    tx.execute(
        "UPDATE channels SET category_id=NULL WHERE category_id=?1",
        [&id],
    )?;
    tx.execute("DELETE FROM categories WHERE id=?1", [&id])?;
    roles::audit(&tx, &auth.user.id, "category.deleted", &id)?;
    tx.commit()?;
    state.notify("channels", None, None, None);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let mut s=db.prepare("SELECT token_hash,created_at,expires FROM sessions WHERE user_id=?1 AND expires>?2 ORDER BY created_at DESC")?;
    let rows=s.query_map(params![auth.user.id,now()],|r|{let id:String=r.get(0)?;Ok(serde_json::json!({"id":id,"created_at":r.get::<_,u64>(1)?,"expires_at":r.get::<_,u64>(2)?,"current":id==auth.token_hash}))})?.collect::<Result<Vec<_>,_>>()?;
    Ok(Json(serde_json::json!(rows)))
}
pub async fn revoke_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    let affected = lock(&state.0.db).execute(
        "DELETE FROM sessions WHERE token_hash=?1 AND user_id=?2",
        params![id, auth.user.id],
    )?;
    if affected == 0 {
        return Err(missing());
    }
    state.remove_voice(&lock(&state.0.db), &auth.user.id, Some(&id))?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    pub current_password: String,
    pub new_password: String,
}
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PasswordChange>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    if !(10..=256).contains(&input.new_password.len()) || input.current_password.len() > 256 {
        return Err(bad("Password must be 10 to 256 bytes"));
    }
    super::password_policy::validate(&auth.user.username, &input.new_password)?;
    let hash: String = lock(&state.0.db).query_row(
        "SELECT password_hash FROM users WHERE id=?1",
        [&auth.user.id],
        |r| r.get(0),
    )?;
    let permit = state
        .0
        .password_jobs
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Sign-in service is busy"))?;
    let mfa_record = super::authenticator::record(&lock(&state.0.db), &auth.user.id)?;
    let mfa_user = auth.user.id.clone();
    let previous_hash = hash.clone();
    let (updated, rewrap) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let current_password = zeroize::Zeroizing::new(input.current_password);
        let new_password = zeroize::Zeroizing::new(input.new_password);
        let valid = PasswordHash::new(&hash).is_ok_and(|h| {
            Argon2::default()
                .verify_password(current_password.as_bytes(), &h)
                .is_ok()
        });
        if !valid {
            return Err(unauthorized());
        }
        let updated = Argon2::default()
            .hash_password(new_password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map(|h| h.to_string())
            .map_err(|_| {
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed")
            })?;
        let rewrap =
            super::authenticator::rewrap(&mfa_user, mfa_record, &current_password, &new_password)?;
        Ok((updated, rewrap))
    })
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Password operation failed",
        )
    })??;
    {
        let mut db = lock(&state.0.db);
        let tx = db.transaction()?;
        let same_password: bool = tx.query_row(
            "SELECT password_hash=?2 FROM users WHERE id=?1",
            params![auth.user.id, previous_hash],
            |row| row.get(0),
        )?;
        if !same_password {
            return Err(unauthorized());
        }
        super::authenticator::apply_rewrap(&tx, &auth.user.id, rewrap.as_ref())?;
        tx.execute(
            "UPDATE users SET password_hash=?2 WHERE id=?1",
            params![auth.user.id, updated],
        )?;
        tx.execute("DELETE FROM sessions WHERE user_id=?1", [&auth.user.id])?;
        tx.commit()?;
    }
    state.remove_voice(&lock(&state.0.db), &auth.user.id, None)?;
    Ok(StatusCode::NO_CONTENT)
}

fn backup_path(state: &AppState, id: &str) -> ApiResult<PathBuf> {
    // Only the plain hyphenated form names a file; "urn:uuid:…" or "{…}" never
    // reach the file system, so no colon or brace can end up in a path.
    let parsed = Uuid::parse_str(id).map_err(|_| bad("Invalid backup identifier"))?;
    if parsed.hyphenated().to_string() != id {
        return Err(bad("Invalid backup identifier"));
    }
    Ok(state.0.data.join("backups").join(format!("{id}.sqlite3")))
}
pub fn backup_database(
    db: &Connection,
    destination: &FsPath,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reserve the exact destination rather than racing an exists check.
    let reservation = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    drop(reservation);
    let result = (|| {
        db.backup(rusqlite::MAIN_DB, destination, None)?;
        let copied = Connection::open(destination)?;
        let check: String = copied.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err("Backup integrity check failed".into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}
pub fn restore_database(
    db: &mut Connection,
    source: &FsPath,
) -> Result<(), Box<dyn std::error::Error>> {
    let incoming = Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: u32 = incoming.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(
            "Restore requires a backup with the current schema; migrate an offline copy first"
                .into(),
        );
    }
    let check: String = incoming.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err("Backup integrity check failed".into());
    }
    let _: String = server_id(&incoming)?;
    let foreign = incoming.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if foreign {
        return Err("Backup foreign key check failed".into());
    }
    rusqlite::backup::Backup::new(&incoming, db)?.run_to_completion(
        128,
        Duration::from_millis(5),
        None,
    )?;
    db.execute("DELETE FROM sessions", [])?;
    // The restored file has another history: no earlier replica cursor is valid.
    crate::replica::after_restore(db)?;
    Ok(())
}
pub async fn backups(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    let dir = state.0.data.join("backups");
    let mut entries = vec![];
    if let Ok(files) = std::fs::read_dir(dir) {
        for entry in files.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "sqlite3") {
                if let Some(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .filter(|s| Uuid::parse_str(s).is_ok())
                {
                    if let Ok(m) = entry.metadata() {
                        entries.push(serde_json::json!({"id":id,"bytes":m.len(),"created_at":m.modified().ok().and_then(|t|t.duration_since(UNIX_EPOCH).ok()).map(|t|t.as_secs())}));
                    }
                }
            }
        }
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry["created_at"].as_u64().unwrap_or(0)));
    entries.truncate(200);
    Ok(Json(serde_json::json!(entries)))
}
pub async fn create_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    std::fs::create_dir_all(state.0.data.join("backups")).map_err(|_| {
        ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Cannot create backup directory",
        )
    })?;
    let id = id();
    let path = backup_path(&state, &id)?;
    backup_database(&lock(&state.0.db), &path).map_err(|_| {
        ApiError::new(
            StatusCode::INSUFFICIENT_STORAGE,
            "Backup failed; check disk space and server logs",
        )
    })?;
    Ok(Json(
        serde_json::json!({"id":id,"bytes":std::fs::metadata(path).map(|m|m.len()).unwrap_or(0),"created_at":now()}),
    ))
}
const MAX_BACKUP_BYTES: u64 = 4 * 1024 * 1024 * 1024;
async fn backup_response(path: PathBuf) -> ApiResult<Response> {
    let file = tokio::fs::File::open(&path).await.map_err(|_| missing())?;
    let bytes = file.metadata().await.map_err(|_| missing())?.len();
    if bytes > MAX_BACKUP_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Backup exceeds the4GiB download limit; use offline backup",
        ));
    }
    let mut response =
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024))
            .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/vnd.sqlite3"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        axum::http::HeaderValue::from_static("attachment; filename=relay-backup.sqlite3"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        axum::http::HeaderValue::from_str(&bytes.to_string())
            .map_err(|_| bad("Invalid backup size"))?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}
pub async fn download_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    backup_response(backup_path(&state, &id)?).await
}
pub async fn snapshot_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Json(metadata) = create_backup(State(state.clone()), headers).await?;
    let id = metadata["id"].as_str().ok_or_else(missing)?;
    backup_response(backup_path(&state, id)?).await
}
pub async fn admin_metadata(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    let db = lock(&state.0.db);
    Ok(Json(
        serde_json::json!({"server_id":server_id(&db)?,"instance_id":state.0.instance_id,"managed_instance":true,"pid":std::process::id(),"schema_version":SCHEMA_VERSION,"protocol_version":API_PROTOCOL_VERSION,"server_name":crate::server_settings::presentation(&db, &state.0.name)?.name,"storage_limit_bytes":state.0.config.storage_limit_bytes,"user_storage_limit_bytes":state.0.config.user_storage_limit_bytes,"rtc_enabled":state.0.config.rtc.is_some()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownRequest {
    pub instance_id: String,
    pub confirm_server_id: String,
}
pub async fn admin_shutdown(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ShutdownRequest>,
) -> ApiResult<StatusCode> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    if input.instance_id != state.0.instance_id
        || input.confirm_server_id != server_id(&lock(&state.0.db))?
    {
        return Err(bad("Server instance changed; refresh before stopping"));
    }
    state.request_shutdown();
    Ok(StatusCode::NO_CONTENT)
}
impl AppState {
    pub fn request_shutdown(&self) {
        self.0.shutdown.send_replace(true);
    }
    pub async fn shutdown_requested(&self) {
        let mut shutdown = self.0.shutdown.subscribe();
        while !*shutdown.borrow_and_update() {
            if shutdown.changed().await.is_err() {
                break;
            }
        }
    }
    pub fn instance_id(&self) -> &str {
        &self.0.instance_id
    }
    pub fn server_id(&self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(server_id(&lock(&self.0.db))?)
    }
    pub fn has_users(&self) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(
            lock(&self.0.db).query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))?
                > 0,
        )
    }
    pub fn backup_to(&self, destination: &FsPath) -> Result<(), Box<dyn std::error::Error>> {
        backup_database(&lock(&self.0.db), destination)
    }
    pub fn restore_from(&self, source: &FsPath) -> Result<(), Box<dyn std::error::Error>> {
        if lock(&self.0.takeover).is_some() {
            return Err("Protected hosting must be stopped before restoring a different database".into());
        }
        if !self.writes_allowed() {
            return Err("Cancel handover preparation before restoring a database".into());
        }
        std::fs::create_dir_all(self.0.data.join("backups"))?;
        let prepared = self
            .0
            .data
            .join(format!("restore-staging-{}.sqlite3", id()));
        std::fs::copy(source, &prepared)?;
        let result = (|| {
            let mut copy = Connection::open(&prepared)?;
            copy.execute_batch("PRAGMA foreign_keys=ON;")?;
            migrate_database(&mut copy)?;
            drop(copy);
            backup_database(
                &lock(&self.0.db),
                &self
                    .0
                    .data
                    .join("backups")
                    .join(format!("{}.sqlite3", id())),
            )?;
            let mut db = lock(&self.0.db);
            let incoming =
                Connection::open_with_flags(&prepared, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            self.0
                .handover
                .begin_restore(server_id(&incoming)?)
                .map_err(|error| Box::<dyn std::error::Error>::from(error.message))?;
            self.0
                .handover_exports
                .recover(&self.0.data, &self.0.handover.status());
            drop(incoming);
            let result = restore_database(&mut db, &prepared);
            self.0
                .handover
                .stamp_database(&db)
                .map_err(|error| Box::<dyn std::error::Error>::from(error.message))?;
            self.0
                .handover
                .finish_restore(server_id(&db)?)
                .map_err(|error| Box::<dyn std::error::Error>::from(error.message))?;
            result
        })();
        let _ = std::fs::remove_file(&prepared);
        result
    }
    pub fn initialize_admin(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.0.config.people {
            return Err("Friends use Relay accounts, not a community administrator".into());
        }
        validate_auth(&AuthRequest {
            second_factor: None,
            initial_protection: None,
            username: username.to_owned(),
            password: password.to_owned(),
            invite: String::new(),
        })
        .map_err(|e| e.message)?;
        super::password_policy::validate(username, password).map_err(|e| e.message)?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map_err(|_| "Password hashing failed")?
            .to_string();
        let mut db = lock(&self.0.db);
        let tx = db.transaction()?;
        if tx.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, u32>(0))? > 0 {
            return Err("Administrator already initialized".into());
        }
        let user = id();
        tx.execute(
            "INSERT INTO users VALUES(?1,?2,?3,'admin')",
            params![user, username.trim(), hash],
        )?;
        tx.execute("INSERT INTO membership VALUES(?1,'active')", [&user])?;
        tx.execute("INSERT INTO user_profiles(user_id) VALUES(?1)", [&user])?;
        tx.commit()?;
        Ok(())
    }
    pub fn recover_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.0.config.people {
            return Err("Manage this account at the Relay account provider".into());
        }
        if !(10..=256).contains(&password.len()) {
            return Err("Password must be10..256bytes".into());
        }
        super::password_policy::validate(username, password).map_err(|e| e.message)?;
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng))
            .map_err(|_| "Password hashing failed")?
            .to_string();
        let mut db = lock(&self.0.db);
        let tx = db.transaction()?;
        let user: String = tx.query_row(
            "SELECT id FROM users WHERE username=?1 COLLATE NOCASE AND role='admin'",
            [username],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE users SET password_hash=?2 WHERE id=?1",
            params![user, hash],
        )?;
        let status: String = tx.query_row(
            "SELECT status FROM membership WHERE user_id=?1",
            [&user],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE membership SET status='active' WHERE user_id=?1",
            [&user],
        )?;
        if status != "active" {
            crate::server_settings::record_system_event(&tx, &user, "joined")?;
        }
        tx.execute(
            "DELETE FROM authenticator_credentials WHERE user_id=?1",
            [&user],
        )?;
        tx.execute("DELETE FROM authenticator_setup WHERE user_id=?1", [&user])?;
        tx.execute("DELETE FROM sessions WHERE user_id=?1", [user])?;
        tx.commit()?;
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub backup_id: String,
    pub confirm_server_id: String,
}
pub async fn restore_backup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RestoreRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    require_admin(&auth)?;
    let source = backup_path(&state, &input.backup_id)?;
    {
        let mut db = lock(&state.0.db);
        if server_id(&db)? != input.confirm_server_id {
            return Err(bad("Confirm this server's identifier before restoring"));
        }
        std::fs::create_dir_all(state.0.data.join("backups")).map_err(|_| {
            ApiError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "Cannot save recovery backup",
            )
        })?;
        backup_database(&db, &backup_path(&state, &id())?).map_err(|_| {
            ApiError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "Cannot create recovery backup; restore cancelled",
            )
        })?;
        let incoming =
            Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        state.0.handover.begin_restore(server_id(&incoming)?)?;
        state
            .0
            .handover_exports
            .recover(&state.0.data, &state.0.handover.status());
        drop(incoming);
        let result = restore_database(&mut db, &source);
        state.0.handover.stamp_database(&db)?;
        state.0.handover.finish_restore(server_id(&db)?)?;
        result.map_err(|_| bad("Restore failed: backup must be valid and match this schema"))?;
    }
    state.revoke_all_rtc();
    lock(&state.0.voice).clear();
    lock(&state.0.frames).clear();
    lock(&state.0.typing).clear();
    state.notify("refresh", None, None, None);
    Ok(Json(
        serde_json::json!({"restored":true,"sign_in_required":true}),
    ))
}
pub async fn storage(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let auth = state.auth(&headers)?;
    let db = lock(&state.0.db);
    let (total, used) = profiles::storage_usage(&db, &auth.user.id)?;
    Ok(Json(
        serde_json::json!({"user_bytes":used,"user_limit_bytes":state.0.config.user_storage_limit_bytes,"server_bytes":if auth.user.role=="admin"{Some(total)}else{None},"server_limit_bytes":state.0.config.storage_limit_bytes}),
    ))
}

/// Bounded blocking workers isolate synchronous SQLite/filesystem work from Tokio IO.
/// Security changes serialize with outstanding requests so stale Auth values cannot
/// commit after a permission/session revocation. Guard/body waits have bounded timeouts.
pub async fn dispatch(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let permit = match state.0.request_jobs.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server is busy. Retry shortly",
            )
            .into_response()
        }
    };
    let path = uri.path();
    let mutation = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    let exclusive = mutation
        && (matches!(
            path,
            "/api/admin/restore"
                | "/api/logout"
                | "/api/password"
                | "/api/voice/join"
                | "/api/voice/resume"
                | "/api/voice/leave"
        ) || path.starts_with("/api/members/")
            || path.starts_with("/api/channels/")
            || path.starts_with("/api/sessions/")
            || (path.starts_with("/api/hosting/handover/")
                && !matches!(
                    path,
                    "/api/hosting/handover/export" | "/api/hosting/handover/part"
                )));
    let admission_path = path.to_owned();
    let handle = tokio::runtime::Handle::current();
    let work_state = state.clone();
    let run = move || {
        handle.block_on(async move {
            let _permit = permit;
            if work_state.takeover_blocked() {
                return ApiError::new(StatusCode::SERVICE_UNAVAILABLE,
                    "Protected host is paused while its copies reconnect or recover").into_response();
            }
            if let Err(error) = work_state.0.handover.admit(&admission_path) {
                return error.into_response();
            }
            let mut response =
                match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
                    Ok(response) => response,
                    Err(_) => {
                        return ApiError::new(
                            StatusCode::REQUEST_TIMEOUT,
                            "Request timed out. Retry with the same message client_id",
                        )
                        .into_response()
                    }
                };
            if mutation && work_state.writes_allowed() {
                if exclusive {
                    chat_controls::prune_typing(&work_state);
                }
                work_state.enforce_rtc_permissions();
                if work_state.flush_rtc_removals().await.is_err() && response.status().is_success()
                {
                    response = ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "Local access revoked; video disconnect pending. Retry shortly",
                    )
                    .into_response();
                }
            }
            if work_state.takeover_blocked() {
                return ApiError::new(StatusCode::SERVICE_UNAVAILABLE,
                    "Protected write is not confirmed. Retry with the same message client_id").into_response();
            }
            response
        })
    };
    let worker = if exclusive {
        let guard = match tokio::time::timeout(
            Duration::from_secs(5),
            state.0.maintenance.clone().write_owned(),
        )
        .await
        {
            Ok(g) => g,
            Err(_) => {
                return ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server maintenance is busy. Retry shortly",
                )
                .into_response()
            }
        };
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            run()
        })
    } else {
        let guard = match tokio::time::timeout(
            Duration::from_secs(5),
            state.0.maintenance.clone().read_owned(),
        )
        .await
        {
            Ok(g) => g,
            Err(_) => {
                return ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Server maintenance is busy. Retry shortly",
                )
                .into_response()
            }
        };
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            run()
        })
    };
    match worker.await {
        Ok(response) => response,
        Err(_) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Request worker failed")
            .into_response(),
    }
}

pub async fn local_shutdown(
    State(state): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(input): Json<ShutdownRequest>,
) -> ApiResult<StatusCode> {
    if !remote.ip().is_loopback() {
        return Err(forbidden());
    }
    let expected = state
        .0
        .config
        .local_control_token
        .as_deref()
        .ok_or_else(forbidden)?;
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    if token_hash(token) != token_hash(expected) {
        return Err(unauthorized());
    }
    if input.instance_id != state.0.instance_id
        || input.confirm_server_id != server_id(&lock(&state.0.db))?
    {
        return Err(bad("Server instance changed; refresh before stopping"));
    }
    state.request_shutdown();
    Ok(StatusCode::NO_CONTENT)
}
