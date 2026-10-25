//! Slice 1 (static checks and characterization) and slice 2 (migration 24, log).
//! All of it runs on the packaged SQLite with the server's real connection
//! setup (WAL, foreign keys on, `recursive_triggers` left at its default).
use super::*;
use std::collections::BTreeSet;

fn open(dir: &tempfile::TempDir) -> Connection {
    let db = Connection::open(dir.path().join("replica.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    // Exactly the initialization in `Server::open`.
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .unwrap();
    db
}

fn migrated() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = open(&dir);
    crate::community::migrate_database(&mut db).unwrap();
    (dir, db)
}

fn enabled() -> (tempfile::TempDir, Connection) {
    let (dir, db) = migrated();
    enable(&db).unwrap();
    (dir, db)
}

fn log(db: &Connection) -> Vec<(String, String, String)> {
    let mut statement = db
        .prepare("SELECT tbl,op,pk FROM replica_log ORDER BY seq")
        .unwrap();
    let rows = statement
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn entry(table: &str, op: &str, pk: &str) -> (String, String, String) {
    (table.into(), op.into(), pk.into())
}

fn exec(db: &Connection, sql: &str) {
    db.execute_batch(sql).unwrap();
}

fn clear_log(db: &Connection) {
    db.execute("DELETE FROM replica_log", []).unwrap();
}

fn user(db: &Connection, id: &str) {
    db.execute(
        "INSERT INTO users(id,username,password_hash,role) VALUES(?1,?1,'hash','member')",
        [id],
    )
    .unwrap();
    db.execute("INSERT INTO membership(user_id) VALUES(?1)", [id])
        .unwrap();
}

fn channel(db: &Connection, id: &str) {
    db.execute(
        "INSERT INTO channels(id,name,kind,topic) VALUES(?1,?1,'text','')",
        [id],
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Slice 1

#[test]
fn every_table_and_column_is_classified() {
    let (_dir, db) = migrated();
    let mut names = BTreeSet::new();
    let mut statement = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'")
        .unwrap();
    for name in statement.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        names.insert(name.unwrap());
    }
    let unclassified: Vec<_> = names.iter().filter(|n| table_for(n).is_none()).collect();
    assert!(
        unclassified.is_empty(),
        "classify these tables in replica::TABLES: {unclassified:?}"
    );
    for table in TABLES {
        assert!(
            names.contains(table.name),
            "{} is classified but not in the schema",
            table.name
        );
    }
    for table in TABLES {
        if table.class.logged() {
            assert!(
                !table.key.is_empty() && !table.columns.is_empty(),
                "{}",
                table.name
            );
        } else {
            assert!(
                table.key.is_empty() && table.columns.is_empty() && table.conditional.is_empty(),
                "{} is not exported, so it lists no columns",
                table.name
            );
            continue;
        }
        // Every real column is exactly one of: exported, conditional, held back.
        let mut statement = db
            .prepare(&format!("PRAGMA table_info({})", table.name))
            .unwrap();
        let info = statement
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let real: BTreeSet<_> = info.iter().map(|(n, _)| n.as_str()).collect();
        let listed: Vec<_> = table
            .columns
            .iter()
            .chain(table.conditional)
            .chain(table.held_back)
            .copied()
            .collect();
        let unique: BTreeSet<_> = listed.iter().copied().collect();
        assert_eq!(
            listed.len(),
            unique.len(),
            "{}: a column is listed twice",
            table.name
        );
        assert_eq!(real, unique, "{}: classify every column", table.name);
        // The log key is the declared primary key, in order.
        let mut key: Vec<_> = info.iter().filter(|(_, pk)| *pk > 0).collect();
        key.sort_by_key(|(_, pk)| *pk);
        let key: Vec<_> = key.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            key, table.key,
            "{}: key differs from the primary key",
            table.name
        );
    }
}

fn source_files() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut paths = Vec::new();
    walk(&root, &mut paths);
    paths
        .into_iter()
        // This module quotes the statements it looks for.
        .filter(|p| {
            !p.components().any(|c| c.as_os_str() == "replica")
                && p.file_name().unwrap() != "replica.rs"
        })
        .map(|p| {
            let text = std::fs::read_to_string(&p).unwrap();
            (p.file_name().unwrap().to_string_lossy().into_owned(), text)
        })
        .collect()
}

fn word_after(text: &str, from: usize) -> String {
    text[from..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

#[test]
fn no_unsafe_replace_or_key_update_on_logged_tables() {
    for (file, text) in source_files() {
        let lower = text.to_ascii_lowercase();
        // REPLACE deletes a conflicting row without firing delete triggers
        // (recursive_triggers is off), so it may only touch tables that are not logged.
        for needle in ["or replace into ", "replace into "] {
            for (at, _) in lower.match_indices(needle) {
                let table = word_after(&lower, at + needle.len());
                let logged = table_for(&table).is_some_and(|t| t.class.logged());
                assert!(!logged, "{file}: REPLACE on logged table {table}");
            }
        }
        // An UPDATE must not change primary-key columns of a logged table.
        // (Best effort on the SQL text; the update trigger also writes a
        // tombstone for a changed key, which the tests below prove.)
        for (at, _) in lower.match_indices("update ") {
            let table = word_after(&lower, at + "update ".len());
            let Some(info) = table_for(&table).filter(|t| t.class.logged()) else {
                continue;
            };
            let rest = &lower[at..];
            let Some(set) = rest.find(" set ") else {
                continue;
            };
            if set > 80 {
                continue;
            }
            let clause: String = rest[set + 5..]
                .chars()
                .take_while(|c| *c != '"' && *c != ';')
                .collect();
            let clause = clause.split(" where ").next().unwrap_or_default();
            for assignment in clause.split(',') {
                let column = assignment.split('=').next().unwrap_or_default().trim();
                assert!(
                    !info.key.contains(&column),
                    "{file}: UPDATE changes key column {column} of {table}"
                );
            }
        }
    }
}

#[test]
fn crypto_columns_are_classified_and_foreign_bodies_withheld() {
    let events = table_for("crypto_events").unwrap();
    assert!(
        events.conditional.contains(&"body"),
        "bodies leave only for groups the recipient is in"
    );
    assert!(!events.columns.contains(&"body"));
    let devices = table_for("crypto_devices").unwrap();
    assert!(devices.held_back.contains(&"key_package"));
    for name in [
        "crypto_key_packages",
        "crypto_welcome_packages",
        "crypto_key_package_consumptions",
    ] {
        assert_eq!(table_for(name).unwrap().class, Class::Withheld, "{name}");
    }
    assert_eq!(
        table_for("crypto_call_requests").unwrap().class,
        Class::Ephemeral
    );
    // What this proves is classification and a size bound. It cannot prove that
    // opaque client bytes contain no secret; that is why foreign bodies are withheld.
    assert_eq!(relay_protocol::CRYPTO_MAX_EVENT_DECODED_BYTES, 512 * 1024);
}

#[test]
fn secrets_and_withheld_stores_are_never_logged_or_exported() {
    for name in [
        "sessions",
        "authenticator_credentials",
        "authenticator_setup",
        "authenticator_recovery",
        "identity_link_challenges",
        "identity_login_challenges",
        "voice_resume_leases",
        "private_call_recovery",
        "replica_state",
        "replica_log",
        "replica_tokens",
    ] {
        assert_eq!(table_for(name).unwrap().class, Class::Never, "{name}");
    }
    for name in [
        "invites",
        "member_invitations",
        "linked_identities",
        "friendships",
        "user_blocks",
        "user_reports",
        "audit_log",
    ] {
        assert_eq!(table_for(name).unwrap().class, Class::Withheld, "{name}");
    }
    assert!(table_for("users")
        .unwrap()
        .held_back
        .contains(&"password_hash"));
    assert!(!METADATA_KEYS.contains(&"open_call_secret"));
    // No trigger exists on any table that is not logged.
    let defs = trigger_definitions();
    for (_, sql) in &defs {
        for table in TABLES.iter().filter(|t| !t.class.logged()) {
            assert!(
                !sql.contains(&format!(" ON {} ", table.name)),
                "trigger on unlogged {}",
                table.name
            );
        }
    }
}

// Characterization of the packaged SQLite. These pin facts the design relies on
// without turning on `recursive_triggers`, which would change every trigger.
const SCRATCH: &str = "
    CREATE TABLE p(id TEXT PRIMARY KEY, u TEXT UNIQUE);
    CREATE TABLE c(id TEXT PRIMARY KEY, p TEXT REFERENCES p(id) ON DELETE CASCADE);
    CREATE TABLE s(id TEXT PRIMARY KEY, p TEXT REFERENCES p(id) ON DELETE SET NULL);
    CREATE TABLE lg(seq INTEGER PRIMARY KEY AUTOINCREMENT, t TEXT, op TEXT, k TEXT);
    CREATE TRIGGER p_i AFTER INSERT ON p BEGIN INSERT INTO lg(t,op,k) VALUES('p','i',new.id); END;
    CREATE TRIGGER p_u AFTER UPDATE ON p BEGIN INSERT INTO lg(t,op,k) VALUES('p','i',new.id); END;
    CREATE TRIGGER p_d AFTER DELETE ON p BEGIN INSERT INTO lg(t,op,k) VALUES('p','d',old.id); END;
    CREATE TRIGGER c_i AFTER INSERT ON c BEGIN INSERT INTO lg(t,op,k) VALUES('c','i',new.id); END;
    CREATE TRIGGER c_u AFTER UPDATE ON c BEGIN INSERT INTO lg(t,op,k) VALUES('c','i',new.id); END;
    CREATE TRIGGER c_d AFTER DELETE ON c BEGIN INSERT INTO lg(t,op,k) VALUES('c','d',old.id); END;
    CREATE TRIGGER s_i AFTER INSERT ON s BEGIN INSERT INTO lg(t,op,k) VALUES('s','i',new.id); END;
    CREATE TRIGGER s_u AFTER UPDATE ON s BEGIN INSERT INTO lg(t,op,k) VALUES('s','i',new.id); END;
    CREATE TRIGGER s_d AFTER DELETE ON s BEGIN INSERT INTO lg(t,op,k) VALUES('s','d',old.id); END;";

fn scratch() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.execute_batch(SCRATCH).unwrap();
    (dir, db)
}

fn scratch_log(db: &Connection) -> Vec<String> {
    let mut statement = db
        .prepare("SELECT t||op||':'||k FROM lg ORDER BY seq")
        .unwrap();
    let rows = statement
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    rows
}

#[test]
fn packaged_sqlite_trigger_semantics() {
    // (a) REPLACE removes the conflicting row without a delete trigger while
    // recursive_triggers is off; with it on, the delete trigger fires.
    let (_d, db) = scratch();
    exec(
        &db,
        "INSERT INTO p VALUES('a','x'); INSERT OR REPLACE INTO p VALUES('b','x');",
    );
    assert_eq!(
        scratch_log(&db),
        ["pi:a", "pi:b"],
        "no delete record for the replaced row"
    );
    let (_d, db) = scratch();
    db.pragma_update(None, "recursive_triggers", true).unwrap();
    exec(
        &db,
        "INSERT INTO p VALUES('a','x'); INSERT OR REPLACE INTO p VALUES('b','x');",
    );
    assert_eq!(scratch_log(&db), ["pi:a", "pd:a", "pi:b"]);
    // The server does not enable it.
    let (_d, real) = migrated();
    let on: i64 = real
        .pragma_query_value(None, "recursive_triggers", |r| r.get(0))
        .unwrap();
    assert_eq!(on, 0);

    // (b) Foreign-key actions fire the child's triggers: CASCADE as a delete,
    // SET NULL as an update.
    let (_d, db) = scratch();
    exec(
        &db,
        "INSERT INTO p VALUES('a','x'); INSERT INTO c VALUES('c1','a'); INSERT INTO s VALUES('s1','a');
         DELETE FROM lg; DELETE FROM p WHERE id='a';",
    );
    let seen = scratch_log(&db);
    assert!(seen.contains(&"pd:a".to_string()), "{seen:?}");
    assert!(
        seen.contains(&"cd:c1".to_string()),
        "cascade delete fires: {seen:?}"
    );
    assert!(
        seen.contains(&"si:s1".to_string()),
        "set null fires as update: {seen:?}"
    );

    // (c) An ignored insert changes nothing and logs nothing.
    let (_d, db) = scratch();
    exec(
        &db,
        "INSERT INTO p VALUES('a','x'); DELETE FROM lg; INSERT OR IGNORE INTO p VALUES('b','x');",
    );
    assert!(scratch_log(&db).is_empty());

    // (d) Multi-row statements log per row.
    let (_d, db) = scratch();
    exec(&db, "INSERT INTO p VALUES('a','1'),('b','2'),('c','3'); DELETE FROM lg; UPDATE p SET u=u||'!'; DELETE FROM p;");
    assert_eq!(
        scratch_log(&db),
        ["pi:a", "pi:b", "pi:c", "pd:a", "pd:b", "pd:c"]
    );

    // (f) A rolled-back write leaves no record.
    let (_d, mut db) = scratch();
    {
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO p VALUES('a','x')", []).unwrap();
        tx.rollback().unwrap();
    }
    assert!(scratch_log(&db).is_empty());

    // (g) AUTOINCREMENT only increases. Deleting log rows leaves gaps, so a
    // missing number is never read as a lost event.
    let (_d, db) = scratch();
    exec(
        &db,
        "INSERT INTO p VALUES('a','1'),('b','2'),('c','3'); DELETE FROM lg WHERE seq=2;",
    );
    let seqs: Vec<i64> = db
        .prepare("SELECT seq FROM lg ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(seqs, [1, 3]);
}

#[test]
fn invite_id_is_the_invite_code_hash() {
    // `member_invitations.invite_id` is `token_hash(invite)`, the same value as
    // `invites.code_hash`: a verifier, which is why the table is withheld.
    let lib = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
    )
    .unwrap();
    let mut sites = 0;
    for (at, _) in lib.match_indices("INTO member_invitations") {
        sites += 1;
        let after = &lib[at..(at + 220).min(lib.len())];
        assert!(after.contains("token_hash("), "{after}");
    }
    assert!(
        sites >= 2,
        "lib.rs writes member_invitations at {sites} sites"
    );
    let login = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/account_login.rs"),
    )
    .unwrap();
    assert!(login.contains("redeem(&tx, &invite_hash)"));
    assert!(
        login.contains("params![user_id, invite_hash]")
            && login.contains("params![user.id, invite_hash]")
    );
    let redeem = lib
        .find("fn redeem")
        .map(|at| &lib[at..(at + 400).min(lib.len())])
        .or_else(|| {
            login
                .find("fn redeem")
                .map(|at| &login[at..(at + 400).min(login.len())])
        });
    assert!(
        redeem.is_some_and(|body| body.contains("code_hash")),
        "redeem matches invites.code_hash"
    );
    assert_eq!(
        table_for("member_invitations").unwrap().class,
        Class::Withheld
    );
    assert_eq!(table_for("invites").unwrap().class, Class::Withheld);
}

// ---------------------------------------------------------------------------
// Slice 2

#[test]
fn nothing_is_logged_until_enabled() {
    let (_d, db) = migrated();
    assert!(!state(&db).unwrap().enabled);
    user(&db, "u1");
    channel(&db, "c1");
    assert!(log(&db).is_empty());
    assert_eq!(state(&db).unwrap().permission_generation, 0);
    assert!(trigger_set_intact(&db).unwrap());
}

#[test]
fn log_row_exists_exactly_when_an_allow_listed_write_commits() {
    let (_d, db) = enabled();
    user(&db, "u1");
    assert_eq!(
        log(&db),
        [
            entry("users", "i", "[\"u1\"]"),
            entry("membership", "i", "[\"u1\"]")
        ]
    );
    clear_log(&db);
    exec(&db, "UPDATE users SET role='moderator' WHERE id='u1'");
    assert_eq!(log(&db), [entry("users", "i", "[\"u1\"]")]);
    clear_log(&db);
    exec(&db, "DELETE FROM membership WHERE user_id='u1'");
    assert_eq!(log(&db), [entry("membership", "d", "[\"u1\"]")]);
    clear_log(&db);

    // A composite key is logged as its quoted parts.
    channel(&db, "c1");
    clear_log(&db);
    exec(
        &db,
        "INSERT INTO channel_permissions VALUES('c1','role','member',3)",
    );
    assert_eq!(
        log(&db),
        [entry(
            "channel_permissions",
            "i",
            "[\"c1\",\"role\",\"member\"]"
        )]
    );
    clear_log(&db);

    // Updating a primary key leaves a tombstone for the old key and a row for the new one.
    exec(&db, "INSERT INTO categories VALUES('k1','News',0)");
    clear_log(&db);
    exec(&db, "UPDATE categories SET id='k2' WHERE id='k1'");
    assert_eq!(
        log(&db),
        [
            entry("categories", "i", "[\"k2\"]"),
            entry("categories", "d", "[\"k1\"]")
        ]
    );

    // Tables that are not exported leave nothing, even when they change.
    clear_log(&db);
    exec(&db, "INSERT INTO invites(code_hash,expires) VALUES('h',1); INSERT INTO user_presence(user_id,last_seen) VALUES('u1',1);");
    exec(
        &db,
        "INSERT INTO audit_log(actor_id,action,target_id,created_at) VALUES('u1','x','y','z')",
    );
    assert!(log(&db).is_empty());
}

#[test]
fn metadata_logs_only_allow_listed_keys() {
    let (_d, db) = enabled();
    clear_log(&db);
    exec(&db, "INSERT INTO metadata VALUES('open_call_secret','s3cret'); INSERT INTO metadata VALUES('discord-bridge-v1','x'); INSERT INTO metadata VALUES('hosting.v1','{}')");
    exec(
        &db,
        "UPDATE metadata SET value='[]' WHERE key='open_call_secret'",
    );
    exec(
        &db,
        "UPDATE metadata SET value='{\"a\":1}' WHERE key='hosting.v1'",
    );
    assert_eq!(
        log(&db),
        [
            entry("metadata", "i", "[\"hosting.v1\"]"),
            entry("metadata", "i", "[\"hosting.v1\"]")
        ]
    );
    for row in log(&db) {
        assert!(!row.2.contains("secret"));
    }
}

#[test]
fn cascades_replace_ignore_multirow_and_rollback_on_the_real_triggers() {
    let (_d, mut db) = enabled();
    user(&db, "u1");
    channel(&db, "c1");
    exec(&db, "INSERT INTO channel_permissions VALUES('c1','role','member',3),('c1','role','moderator',7)");
    exec(&db, "INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES('m1','c1','u1','hi','t',1)");
    exec(&db, "INSERT INTO reactions VALUES('m1','x','u1')");
    clear_log(&db);

    // Deleting a channel cascades to its permission rows, and each one is logged.
    exec(
        &db,
        "DELETE FROM channel_permissions WHERE channel_id='c1' AND subject_id='moderator'",
    );
    assert_eq!(
        log(&db),
        [entry(
            "channel_permissions",
            "d",
            "[\"c1\",\"role\",\"moderator\"]"
        )]
    );
    clear_log(&db);
    exec(&db, "DELETE FROM messages WHERE id='m1'");
    let seen = log(&db);
    assert!(seen.contains(&entry("messages", "d", "[\"m1\"]")));
    assert!(
        seen.contains(&entry("reactions", "d", "[\"m1\",\"x\",\"u1\"]")),
        "cascaded child row is logged: {seen:?}"
    );

    // Multi-row writes log one row per row; an ignored insert logs nothing.
    clear_log(&db);
    exec(
        &db,
        "INSERT INTO categories VALUES('a','A',0),('b','B',0),('c','C',0)",
    );
    exec(
        &db,
        "INSERT OR IGNORE INTO categories VALUES('a','again',0)",
    );
    exec(&db, "UPDATE categories SET position=5");
    assert_eq!(log(&db).len(), 6);
    assert!(log(&db)
        .iter()
        .all(|(t, op, _)| t == "categories" && op == "i"));

    // A rolled-back write leaves no record and no permission bump.
    clear_log(&db);
    let before = state(&db).unwrap().permission_generation;
    {
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO categories VALUES('z','Z',0)", [])
            .unwrap();
        tx.execute("UPDATE community_roles SET name=name", [])
            .unwrap();
        tx.execute("INSERT INTO community_roles VALUES('r','R',0,1,1)", [])
            .unwrap();
        tx.rollback().unwrap();
    }
    assert!(log(&db).is_empty());
    assert_eq!(state(&db).unwrap().permission_generation, before);
}

#[test]
fn blobs_never_enter_the_log() {
    let (_d, db) = enabled();
    user(&db, "u1");
    channel(&db, "c1");
    exec(&db, "INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES('m1','c1','u1','hi','t',1)");
    clear_log(&db);
    let blob = vec![7u8; 2 * 1024 * 1024];
    db.execute(
        "INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) VALUES('f1','u1','m1','n','image/png',?1,1,?2)",
        params![blob.len() as i64, blob],
    )
    .unwrap();
    assert_eq!(log(&db), [entry("uploads", "i", "[\"f1\"]")]);
    let bytes: i64 = db
        .query_row(
            "SELECT SUM(length(tbl)+length(op)+length(pk)) FROM replica_log",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(bytes < 64, "{bytes}");
    // The log table has no column that could carry row contents.
    let columns: Vec<String> = db
        .prepare("PRAGMA table_info(replica_log)")
        .unwrap()
        .query_map([], |r| r.get(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(columns, ["seq", "tbl", "op", "pk", "at_s"]);
}

#[test]
fn enabling_and_restoring_assign_a_new_lineage_and_a_floor() {
    let (_d, db) = migrated();
    let first = state(&db).unwrap();
    assert!(!first.enabled && first.floor == 0);
    user(&db, "u0");
    enable(&db).unwrap();
    let on = state(&db).unwrap();
    assert!(on.enabled && on.lineage != first.lineage);
    // Idempotent while enabled.
    enable(&db).unwrap();
    assert_eq!(state(&db).unwrap(), on);
    user(&db, "u1");
    channel(&db, "c1");
    let head_before = head(&db).unwrap();
    assert!(head_before >= 3);
    // A restore (or re-enabling) invalidates every cursor: new lineage, floor = head.
    after_restore(&db).unwrap();
    let restored = state(&db).unwrap();
    assert!(restored.enabled);
    assert_ne!(restored.lineage, on.lineage);
    assert_eq!(restored.floor, head_before);
    disable(&db).unwrap();
    let off = state(&db).unwrap();
    assert!(!off.enabled && off.lineage != restored.lineage);
    clear_log(&db);
    user(&db, "u2");
    assert!(log(&db).is_empty());
}

#[test]
fn restore_database_assigns_a_new_lineage() {
    let (dir, mut db) = enabled();
    user(&db, "u1");
    let before = state(&db).unwrap();
    let backup = dir.path().join("backup.sqlite3");
    crate::community::backup_database(&db, &backup).unwrap();
    user(&db, "u2");
    crate::community::restore_database(&mut db, &backup).unwrap();
    let after = state(&db).unwrap();
    assert!(after.enabled);
    assert_ne!(
        after.lineage, before.lineage,
        "a restored history needs a full snapshot"
    );
    assert_eq!(after.floor, head(&db).unwrap());
}

#[test]
fn trigger_set_mismatch_fails_closed() {
    let (_d, db) = enabled();
    assert!(trigger_set_intact(&db).unwrap());
    exec(&db, "DROP TRIGGER replica_categories_u");
    assert!(
        !trigger_set_intact(&db).unwrap(),
        "a missing trigger is detected"
    );
    // Other triggers keep working; only the service would refuse.
    exec(&db, "INSERT INTO categories VALUES('a','A',0)");
    assert_eq!(log(&db), [entry("categories", "i", "[\"a\"]")]);
    // Replacing a trigger body is detected too.
    let (_d, db) = enabled();
    exec(
        &db,
        "DROP TRIGGER replica_categories_i;
         CREATE TRIGGER replica_categories_i AFTER INSERT ON categories BEGIN SELECT 1; END;",
    );
    assert!(!trigger_set_intact(&db).unwrap());
    // And a state row that lost its digest is not trusted.
    let (_d, db) = migrated();
    exec(&db, "UPDATE replica_state SET trigger_set_sha256=''");
    assert!(!trigger_set_intact(&db).unwrap());
}

#[test]
fn permission_generation_changes_on_every_listed_table() {
    let (_d, db) = enabled();
    user(&db, "u1");
    channel(&db, "c1");
    exec(&db, "INSERT INTO community_roles VALUES('r1','R',0,1,3)");
    let mut last = state(&db).unwrap().permission_generation;
    let mut bumped = |sql: &str, label: &str| {
        exec(&db, sql);
        let now = state(&db).unwrap().permission_generation;
        assert!(now > last, "{label} must bump the permission generation");
        last = now;
    };
    bumped(
        "INSERT INTO channel_permissions VALUES('c1','role','member',3)",
        "channel_permissions",
    );
    bumped(
        "UPDATE community_roles SET permissions=7 WHERE id='r1'",
        "community_roles",
    );
    bumped("INSERT INTO member_roles VALUES('u1','r1')", "member_roles");
    bumped(
        "INSERT INTO conversations(channel_id,kind,owner_id) VALUES('c1','group','u1')",
        "conversations",
    );
    bumped(
        "INSERT INTO conversation_members VALUES('c1','u1')",
        "conversation_members",
    );
    bumped(
        "UPDATE membership SET status='left' WHERE user_id='u1'",
        "membership.status",
    );
    bumped(
        "UPDATE channels SET visibility='private' WHERE id='c1'",
        "channels.visibility",
    );
    bumped("UPDATE users SET role='admin' WHERE id='u1'", "users.role");
    // Things that cannot change who may read what leave it alone.
    let steady = state(&db).unwrap().permission_generation;
    exec(&db, "UPDATE channels SET topic='new topic' WHERE id='c1'");
    exec(&db, "UPDATE users SET password_hash='other' WHERE id='u1'");
    exec(&db, "INSERT INTO categories VALUES('k','K',0)");
    assert_eq!(state(&db).unwrap().permission_generation, steady);
}

#[test]
fn retention_precedence_and_floor() {
    let (_d, db) = enabled();
    let insert = |count: i64, at: i64| {
        let tx = db.unchecked_transaction().unwrap();
        let mut statement = tx
            .prepare("INSERT INTO replica_log(tbl,op,pk,at_s) VALUES('categories','i','x',?1)")
            .unwrap();
        for _ in 0..count {
            statement.execute([at]).unwrap();
        }
        drop(statement);
        tx.commit().unwrap();
    };
    let now = 2_000_000_000;
    // Old rows below MIN_ENTRIES are kept, however old.
    insert(10, now - 3 * RETENTION_SECS);
    assert_eq!(prune(&db, now).unwrap(), 0);
    // Within the window, rows above MIN_ENTRIES are kept up to the hard cap.
    insert(MIN_ENTRIES + 5, now - 60);
    let total: i64 = db
        .query_row("SELECT COUNT(*) FROM replica_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, MIN_ENTRIES + 15);
    // recent = MIN_ENTRIES+5, so keep that many: the ten old rows go, and floor rises.
    let removed = prune(&db, now).unwrap();
    assert_eq!(removed, 10);
    let st = state(&db).unwrap();
    assert_eq!(st.floor, 10, "floor is the newest pruned sequence");
    let left: i64 = db
        .query_row("SELECT COUNT(*) FROM replica_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, MIN_ENTRIES + 5);
    // The head does not fall when rows are pruned.
    assert!(head(&db).unwrap() >= MIN_ENTRIES + 15);
    // Pruning twice changes nothing.
    assert_eq!(prune(&db, now).unwrap(), 0);
}

#[test]
fn the_hard_cap_wins_over_the_window() {
    let (_d, db) = enabled();
    let extra = 10;
    let tx = db.unchecked_transaction().unwrap();
    {
        let mut statement = tx
            .prepare(
                "INSERT INTO replica_log(tbl,op,pk,at_s) VALUES('categories','i','x',2000000000)",
            )
            .unwrap();
        for _ in 0..(HARD_CAP + extra) {
            statement.execute([]).unwrap();
        }
    }
    tx.commit().unwrap();
    // Every row is inside the window, yet only the hard cap is kept.
    assert_eq!(prune(&db, 2_000_000_000).unwrap() as i64, extra);
    let left: i64 = db
        .query_row("SELECT COUNT(*) FROM replica_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, HARD_CAP);
    assert_eq!(state(&db).unwrap().floor, extra);
}
