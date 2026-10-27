//! Slice 3 (in process): who gets what, that a catch-up equals a fresh snapshot,
//! and that a replica refuses anything that is not exactly what it asked for.
use super::export::Export;
use super::export::*;
use super::store::*;
use super::*;
use relay_protocol::hosting::{Candidate, CandidateState, HostRole, Roster};
use serde_json::{json, Value};
use std::collections::BTreeSet;

struct World {
    _dir: tempfile::TempDir,
    db: Connection,
}

fn exec(db: &Connection, sql: &str) {
    db.execute_batch(sql).unwrap();
}

fn roster(accepted: bool) -> String {
    serde_json::to_string(&Roster {
        schema: 1,
        revision: 1,
        candidates: vec![Candidate {
            user_id: "r1".into(),
            state: CandidateState::Approved,
            offered: vec![HostRole::Replica],
            accepted: if accepted {
                vec![HostRole::Replica]
            } else {
                vec![]
            },
            offered_by: "a1".into(),
            updated_ms: 1,
        }],
    })
    .unwrap()
}

/// A community with a public channel, a private one, a public one that denies
/// the replica member, direct messages, ciphertext, files and secrets.
fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Connection::open(dir.path().join("source.sqlite3")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .unwrap();
    crate::community::migrate_database(&mut db).unwrap();
    enable(&db).unwrap();
    exec(
        &db,
        "INSERT INTO users(id,username,password_hash,role) VALUES
            ('a1','admin','SENTINEL-PASSWORD','admin'),('m1','mia','SENTINEL-PASSWORD','member'),
            ('r1','rita','SENTINEL-PASSWORD','member'),('x1','xavi','SENTINEL-PASSWORD','member');
         INSERT INTO membership(user_id) VALUES('a1'),('m1'),('r1'),('x1');
         INSERT INTO user_profiles(user_id,display_name,bio,version,avatar) VALUES
            ('m1','Mia','hello',2,x'534543524554415641544152'),('r1','Rita','',0,NULL);
         INSERT INTO channels(id,name,kind,topic,visibility) VALUES
            ('pub','pub-name','text','pub-topic','public'),
            ('priv','priv-name','text','priv-topic','private'),
            ('deny','deny-name','text','deny-topic','public');
         INSERT INTO channel_permissions VALUES('deny','user','r1',0);
         INSERT INTO messages(id,channel_id,recipient_id,author_id,content,created_at,created) VALUES
            ('m_pub','pub',NULL,'m1','PLAIN-PUB','t',1),
            ('m_priv','priv',NULL,'m1','PLAIN-PRIV','t',2),
            ('m_priv_c','priv',NULL,'m1','relay-e2ee:v1:CIPHER-PRIV','t',3),
            ('m_deny','deny',NULL,'m1','PLAIN-DENY','t',4);",
    );
    exec(
        &db,
        "INSERT INTO messages(id,channel_id,recipient_id,author_id,content,created_at,created) VALUES
            ('dm_other',NULL,'x1','m1','PLAIN-DM-OTHER','t',5),
            ('dm_other_c',NULL,'x1','m1','relay-e2ee:v1:CIPHER-DM','t',6),
            ('dm_to_r1',NULL,'r1','m1','PLAIN-DM-TO-R1','t',7);
         INSERT INTO message_order(message_id) VALUES('m_pub'),('m_priv'),('m_priv_c'),('m_deny'),('dm_other'),('dm_other_c'),('dm_to_r1');
         INSERT INTO reactions VALUES('m_pub','x','x1'),('m_priv','x','x1');
         INSERT INTO channel_pins VALUES('m_pub','pub','m1','t',1),('m_priv','priv','m1','t',1);
         INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) VALUES
            ('up_pub','m1','m_pub','a.txt','text/plain',7,1,CAST('PUBFILE' AS BLOB)),
            ('up_priv','m1','m_priv','b.txt','text/plain',10,1,CAST('SECRETFILE' AS BLOB));
         INSERT INTO scheduled_events VALUES('ev_priv','party','SENTINEL-EVENT','priv',1,2);
         INSERT INTO read_states VALUES('r1','channel','pub',3),('m1','channel','pub',9);
         INSERT INTO crypto_devices VALUES('d1','m1','id-1','SENTINEL-KEYPKG',1),('d2','x1','id-2','SENTINEL-KEYPKG-2',1);
         INSERT INTO device_attestations VALUES('d2','x1','ATTEST-INVITE-ID','mac2',1);
         INSERT INTO crypto_groups(target_kind,target_id,group_id) VALUES
            ('channel','priv','g_priv'),('channel','pub','g_pub'),('direct','m1:x1','g_dm');
         INSERT INTO crypto_events VALUES
            ('g_priv',1,0,'create','m1','d1',NULL,'BODY-PRIV','op1'),
            ('g_pub',1,0,'create','m1','d1',NULL,'BODY-PUB','op2'),
            ('g_dm',1,0,'create','m1','d1',NULL,'BODY-DM','op3');
         INSERT INTO sessions(token_hash,user_id,expires) VALUES('SENTINEL-SESSION','m1',9999999999);
         INSERT INTO invites(code_hash,expires) VALUES('SENTINEL-INVITE',9999999999);
         INSERT INTO member_invitations VALUES('m1','SENTINEL-INVITE-ID');
         INSERT INTO linked_identities VALUES('m1','https://issuer','SENTINEL-SUBJECT','mia',1);
         INSERT INTO friendships VALUES('m1','x1',1);
         INSERT INTO user_blocks VALUES('m1','x1');
         INSERT INTO user_reports(id,reporter_id,reported_id,reason,created_at) VALUES('rep','m1','x1','SENTINEL-REASON','t');
         INSERT INTO audit_log(actor_id,action,target_id,created_at) VALUES('a1','SENTINEL-AUDIT','m1','t');
         INSERT INTO metadata VALUES('open_call_secret','SENTINEL-CALL-SECRET'),('discord-bridge-v1','SENTINEL-BRIDGE');",
    );
    db.execute(
        "INSERT INTO metadata VALUES('hosting.v1',?1)",
        [roster(true)],
    )
    .unwrap();
    World { _dir: dir, db }
}

fn pull(
    w: &World,
    who: &str,
    after: Option<i64>,
    previous: &BTreeSet<String>,
) -> Result<Export, Refusal> {
    create(
        &w.db,
        &Request {
            recipient: who,
            replica_id: "rep-1",
            after,
            previous_eligible: previous,
            now_ms: 1_000,
        },
    )
}

fn full(w: &World) -> Export {
    pull(w, "r1", None, &BTreeSet::new()).unwrap()
}

fn store_for(w: &World, who: &str) -> Store {
    let server = crate::community::server_id(&w.db).unwrap();
    Store::new(Connection::open_in_memory().unwrap(), &server, who, "rep-1").unwrap()
}

/// Bring a store up to date the way a replica agent would.
fn catch_up(w: &World, store: &mut Store) -> Result<Applied, ApplyError> {
    let export = match store.cursor().unwrap() {
        None => full(w),
        Some(c) => pull(w, "r1", Some(c.head), &c.eligible).unwrap(),
    };
    store.apply(&export, 5)
}

fn cell<'a>(export: &'a Export, table: &str, key: &str, column: &str) -> Option<&'a Value> {
    let data = &export.tables[table];
    let at = data.columns.iter().position(|c| c == column).unwrap();
    data.rows
        .iter()
        .find(|row| row[0] == json!(key))
        .map(|row| &row[at])
}

// ---------------------------------------------------------------------------

#[test]
fn upload_policy_survives_filtered_snapshot_and_delta_replay() {
    let w = world();
    let first_policy = serde_json::to_string(&UploadPolicy::default()).unwrap();
    w.db.execute("INSERT INTO metadata VALUES('upload_policy',?1)", [&first_policy]).unwrap();
    assert_eq!(cell(&full(&w), "metadata", "upload_policy", "value"), Some(&json!(first_policy)));
    let mut live = store_for(&w, "r1");
    catch_up(&w, &mut live).unwrap();
    let before = live.cursor().unwrap().unwrap();
    let changed = serde_json::to_string(&UploadPolicy { revision: 1, default_file_bytes: 0, ..Default::default() }).unwrap();
    w.db.execute("UPDATE metadata SET value=?1 WHERE key='upload_policy'", [&changed]).unwrap();
    let delta = pull(&w, "r1", Some(before.head), &before.eligible).unwrap();
    assert_eq!(cell(&delta, "metadata", "upload_policy", "value"), Some(&json!(changed)));
    catch_up(&w, &mut live).unwrap();
    let mut fresh = store_for(&w, "r1");
    catch_up(&w, &mut fresh).unwrap();
    assert_eq!(live.dump(), fresh.dump());
    exec(&w.db, "DELETE FROM metadata WHERE key='upload_policy'");
    catch_up(&w, &mut live).unwrap();
    let mut fresh = store_for(&w, "r1");
    catch_up(&w, &mut fresh).unwrap();
    assert_eq!(live.dump(), fresh.dump());
}

#[test]
fn a_full_snapshot_follows_the_recipients_real_permissions_not_visibility() {
    let w = world();
    let export = full(&w);
    let m = &export.manifest;
    assert_eq!(
        m.eligible,
        ["pub"],
        "private and explicitly denied channels are not readable"
    );
    assert!(!m.promotion_eligible);
    assert_eq!(m.replica_format, 1);
    assert_eq!(m.schema_version, crate::community::SCHEMA_VERSION);
    assert_eq!(
        (m.recipient_user_id.as_str(), m.replica_id.as_str()),
        ("r1", "rep-1")
    );
    assert_eq!(m.after, None);
    assert_eq!(m.head, head(&w.db).unwrap());

    // Channel names and topics: readable only for readable channels.
    assert_eq!(
        cell(&export, "channels", "pub", "name"),
        Some(&json!("pub-name"))
    );
    assert_eq!(
        cell(&export, "channels", "priv", "name"),
        Some(&Value::Null)
    );
    assert_eq!(
        cell(&export, "channels", "priv", "topic"),
        Some(&Value::Null)
    );
    // A public channel that denies this member is withheld too: visibility is not access.
    assert_eq!(
        cell(&export, "channels", "deny", "name"),
        Some(&Value::Null)
    );

    // Plaintext versus ciphertext versus direct messages.
    let content = |id: &str| cell(&export, "messages", id, "content").cloned();
    assert_eq!(content("m_pub"), Some(json!("PLAIN-PUB")));
    assert_eq!(
        content("m_priv"),
        Some(Value::Null),
        "plaintext of an unreadable channel is a marker"
    );
    assert_eq!(
        content("m_priv_c"),
        Some(json!("relay-e2ee:v1:CIPHER-PRIV")),
        "ciphertext leaves for every target"
    );
    assert_eq!(content("m_deny"), Some(Value::Null));
    assert_eq!(
        content("dm_other"),
        Some(Value::Null),
        "someone else's plaintext DM is a marker"
    );
    assert_eq!(
        content("dm_other_c"),
        Some(json!("relay-e2ee:v1:CIPHER-DM"))
    );
    assert_eq!(
        content("dm_to_r1"),
        Some(json!("PLAIN-DM-TO-R1")),
        "a DM to the recipient is readable"
    );
    // A marker keeps who and when, not the other party.
    assert_eq!(
        cell(&export, "messages", "dm_other", "author_id"),
        Some(&json!("m1"))
    );
    assert_eq!(
        cell(&export, "messages", "dm_other", "recipient_id"),
        Some(&Value::Null)
    );
    // Every message keeps its place in the order, markers included.
    assert_eq!(export.tables["message_order"].rows.len(), 7);

    // Rows that depend on a readable message follow it, and only it.
    let ids = |table: &str| -> Vec<String> {
        export.tables[table]
            .rows
            .iter()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("uploads"), ["up_pub"]);
    assert_eq!(export.tables["reactions"].rows.len(), 1);
    assert_eq!(ids("channel_pins"), ["m_pub"]);
    assert!(export.tables["scheduled_events"].rows.is_empty());
    // Other people's read positions are not readable by a member.
    assert_eq!(export.tables["read_states"].rows.len(), 1);
    assert_eq!(export.tables["read_states"].rows[0][0], json!("r1"));
    // Group handshake bodies only for groups the recipient can reach.
    let body = |g: &str| {
        let data = &export.tables["crypto_events"];
        let at = data.columns.iter().position(|c| c == "body").unwrap();
        data.rows
            .iter()
            .find(|r| r[0] == json!(g))
            .map(|r| r[at].clone())
    };
    assert_eq!(body("g_pub"), Some(json!("BODY-PUB")));
    assert_eq!(body("g_priv"), Some(Value::Null));
    assert_eq!(
        body("g_dm"),
        Some(Value::Null),
        "a direct group of two other people"
    );
    assert!(m.withheld["messages"] == 3 && m.withheld["channels"] == 2);

    // A role override that grants the private channel changes the answer.
    exec(
        &w.db,
        "INSERT INTO channel_permissions VALUES('priv','user','r1',3)",
    );
    let granted = full(&w);
    assert_eq!(granted.manifest.eligible, ["priv", "pub"]);
    assert_eq!(
        cell(&granted, "messages", "m_priv", "content"),
        Some(&json!("PLAIN-PRIV"))
    );
    assert_eq!(
        cell(&granted, "channels", "priv", "name"),
        Some(&json!("priv-name"))
    );
    assert_eq!(granted.tables["uploads"].rows.len(), 2);
    assert_eq!(
        granted.manifest.permission_generation,
        state(&w.db).unwrap().permission_generation
    );
}

#[test]
fn secrets_and_withheld_stores_do_not_appear_anywhere() {
    let w = world();
    let export = full(&w);
    let mut store = store_for(&w, "r1");
    store.apply(&export, 1).unwrap();
    let everything = format!(
        "{}{:?}{:?}",
        serde_json::to_string(&export).unwrap(),
        store.dump(),
        store.cursor().unwrap()
    );
    for secret in [
        "SENTINEL-PASSWORD",
        "SENTINEL-SESSION",
        "SENTINEL-INVITE",
        "SENTINEL-SUBJECT",
        "SENTINEL-REASON",
        "SENTINEL-AUDIT",
        "SENTINEL-CALL-SECRET",
        "SENTINEL-BRIDGE",
        "SENTINEL-KEYPKG",
        "SENTINEL-EVENT",
        "PLAIN-PRIV",
        "PLAIN-DENY",
        "PLAIN-DM-OTHER",
        "BODY-PRIV",
        "BODY-DM",
        // Blobs are never inline: not the avatar, not a file.
        "PUBFILE",
        "SECRETFILE",
        "534543524554415641544152",
        "SECRETAVATAR",
    ] {
        assert!(!everything.contains(secret), "{secret} leaked");
    }
    // The attestation of an active member's device is exported on purpose: the
    // community device directory already shows it to every member.
    assert!(everything.contains("ATTEST-INVITE-ID"));
    // Nothing from a table that is not exported is even named.
    for name in [
        "sessions",
        "invites",
        "member_invitations",
        "linked_identities",
        "friendships",
        "user_blocks",
        "audit_log",
        "crypto_key_packages",
        "replica_log",
        "replica_tokens",
    ] {
        assert!(!export.tables.contains_key(name), "{name}");
    }
    // Only allow-listed metadata keys are present.
    let keys: BTreeSet<_> = export.tables["metadata"]
        .rows
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["hosting.v1".to_string(), "server_id".to_string()])
    );
}

#[test]
fn a_delta_is_the_state_at_the_pinned_head_not_a_prefix() {
    let w = world();
    let first = full(&w);
    let base = first.manifest.head;
    // The review's counterexample: A@1, B@2, A@3.
    exec(&w.db, "INSERT INTO categories VALUES('A','first',0)");
    exec(&w.db, "INSERT INTO categories VALUES('B','second',0)");
    exec(&w.db, "UPDATE categories SET name='third' WHERE id='A'");
    for after in [base, base + 1, base + 2] {
        let delta = pull(
            &w,
            "r1",
            Some(after),
            &first.manifest.eligible.iter().cloned().collect(),
        )
        .unwrap();
        assert_eq!(
            delta.manifest.head,
            base + 3,
            "no request can return an earlier head"
        );
        assert_eq!(delta.manifest.after, Some(after));
    }
    let delta = pull(
        &w,
        "r1",
        Some(base),
        &first.manifest.eligible.iter().cloned().collect(),
    )
    .unwrap();
    let rows = &delta.tables["categories"].rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], json!("third"), "A carries its newest value");
    assert_eq!(rows[1][1], json!("second"));
    // Nothing changed: an empty delta at the same head is allowed.
    let none = pull(
        &w,
        "r1",
        Some(base + 3),
        &BTreeSet::from(["pub".to_string()]),
    )
    .unwrap();
    assert!(none
        .tables
        .values()
        .all(|t| t.rows.is_empty() && t.tombstones.is_empty()));
}

#[test]
fn catching_up_gives_exactly_what_a_fresh_snapshot_gives() {
    let w = world();
    let mut live = store_for(&w, "r1");
    catch_up(&w, &mut live).unwrap();
    let steps: [&[&str]; 6] = [
        // new, edited and deleted messages (the delete cascades to reactions and pins)
        &[
            "INSERT INTO messages(id,channel_id,author_id,content,created_at,created) VALUES('m_new','pub','m1','NEW','t',8)",
            "INSERT INTO message_order(message_id) VALUES('m_new')",
            "UPDATE messages SET content='EDITED',edited_at='t2' WHERE id='m_pub'",
            "DELETE FROM messages WHERE id='m_priv_c'",
        ],
        // structure: a changed primary key, a new role, a channel rename, metadata
        &[
            "INSERT INTO categories VALUES('k1','News',0)",
            "UPDATE categories SET id='k2' WHERE id='k1'",
            "INSERT INTO community_roles VALUES('role1','Helpers',0,1,3)",
            "INSERT INTO member_roles VALUES('r1','role1')",
            "UPDATE channels SET name='renamed',topic='t2' WHERE id='pub'",
            "INSERT INTO metadata VALUES('presentation','{}')",
        ],
        // the member gains a private channel
        &["INSERT INTO channel_permissions VALUES('priv','user','r1',3)"],
        // and later loses the public one
        &["INSERT INTO channel_permissions VALUES('pub','user','r1',0)"],
        // a left member, a new file, a profile change
        &[
            "UPDATE membership SET status='left' WHERE user_id='x1'",
            "INSERT INTO uploads(id,owner_id,message_id,name,mime,size,created,data) VALUES('up2','m1','m_priv','c','t',3,1,CAST('abc' AS BLOB))",
            "UPDATE user_profiles SET bio='changed',version=3 WHERE user_id='m1'",
        ],
        // access comes back
        &["DELETE FROM channel_permissions WHERE channel_id='pub' AND subject_id='r1'"],
    ];
    for (n, step) in steps.iter().enumerate() {
        for sql in *step {
            exec(&w.db, sql);
        }
        catch_up(&w, &mut live).unwrap();
        let mut fresh = store_for(&w, "r1");
        catch_up(&w, &mut fresh).unwrap();
        assert_eq!(
            live.dump(),
            fresh.dump(),
            "step {n}: a catch-up must equal a fresh snapshot"
        );
        let (a, b) = (
            live.cursor().unwrap().unwrap(),
            fresh.cursor().unwrap().unwrap(),
        );
        assert_eq!((a.head, a.eligible), (b.head, b.eligible), "step {n}");
    }
}

#[test]
fn losing_access_redacts_and_removes_what_was_copied() {
    let w = world();
    let mut store = store_for(&w, "r1");
    catch_up(&w, &mut store).unwrap();
    let before = store.dump();
    assert!(format!("{before:?}").contains("PLAIN-PUB"));
    assert_eq!(before["channel_pins"].len(), 1);
    exec(
        &w.db,
        "INSERT INTO channel_permissions VALUES('pub','user','r1',0)",
    );
    catch_up(&w, &mut store).unwrap();
    let after = store.dump();
    assert!(
        !format!("{after:?}").contains("PLAIN-PUB"),
        "plaintext of a channel the member can no longer read is replaced by a marker"
    );
    assert!(after["channel_pins"].is_empty() && after["uploads"].is_empty());
    assert!(!format!("{after:?}").contains("pub-name"));
    assert_eq!(
        store.cursor().unwrap().unwrap().eligible,
        BTreeSet::<String>::new()
    );
}

#[test]
fn a_wrong_previous_set_can_never_widen_access() {
    let w = world();
    let first = full(&w);
    let base = first.manifest.head;
    exec(&w.db, "INSERT INTO categories VALUES('A','x',0)");
    // Claiming to hold everything, or nothing, changes only what is re-sent.
    for claimed in [
        BTreeSet::from(["pub".to_string(), "priv".to_string(), "deny".to_string()]),
        BTreeSet::new(),
    ] {
        let delta = pull(&w, "r1", Some(base), &claimed).unwrap();
        let text = serde_json::to_string(&delta).unwrap();
        assert!(
            !text.contains("PLAIN-PRIV")
                && !text.contains("PLAIN-DENY")
                && !text.contains("BODY-PRIV")
        );
        assert_eq!(delta.manifest.eligible, ["pub"]);
    }
    // Claiming nothing re-sends the readable channel's content.
    let again = pull(&w, "r1", Some(base), &BTreeSet::new()).unwrap();
    assert!(again.tables["messages"]
        .rows
        .iter()
        .any(|r| r.contains(&json!("PLAIN-PUB"))));
}

#[test]
fn the_source_refuses_instead_of_answering_with_less() {
    let w = world();
    let none = BTreeSet::new();
    // Not a copy holder: not offered, not accepted, other member, left member.
    assert_eq!(
        pull(&w, "x1", None, &none).unwrap_err(),
        Refusal::NotAReplica
    );
    w.db.execute(
        "UPDATE metadata SET value=?1 WHERE key='hosting.v1'",
        [roster(false)],
    )
    .unwrap();
    assert_eq!(
        pull(&w, "r1", None, &none).unwrap_err(),
        Refusal::NotAReplica,
        "an offer that was not accepted grants nothing"
    );
    w.db.execute(
        "UPDATE metadata SET value=?1 WHERE key='hosting.v1'",
        [roster(true)],
    )
    .unwrap();
    assert!(pull(&w, "r1", None, &none).is_ok());
    exec(
        &w.db,
        "UPDATE membership SET status='left' WHERE user_id='r1'",
    );
    assert_eq!(
        pull(&w, "r1", None, &none).unwrap_err(),
        Refusal::NotAReplica
    );
    exec(
        &w.db,
        "UPDATE membership SET status='active' WHERE user_id='r1'",
    );

    // Cursor limits.
    let head_now = head(&w.db).unwrap();
    assert_eq!(
        pull(&w, "r1", Some(head_now + 1), &none).unwrap_err(),
        Refusal::Diverged
    );
    exec(&w.db, &format!("UPDATE replica_state SET floor={head_now}"));
    assert_eq!(
        pull(&w, "r1", Some(head_now - 1), &none).unwrap_err(),
        Refusal::Stale
    );
    assert!(pull(&w, "r1", Some(head_now), &none).is_ok());

    // Too many changed keys: a full snapshot is needed, never a truncated delta.
    {
        let tx = w.db.unchecked_transaction().unwrap();
        let mut statement = tx
            .prepare("INSERT INTO replica_log(tbl,op,pk,at_s) VALUES('categories','i',?1,1)")
            .unwrap();
        for n in 0..=MAX_DELTA_KEYS {
            statement.execute([format!("[\"k{n}\"]")]).unwrap();
        }
        drop(statement);
        tx.commit().unwrap();
    }
    assert_eq!(
        pull(&w, "r1", Some(head_now), &none).unwrap_err(),
        Refusal::TooLarge
    );
    exec(&w.db, "DELETE FROM replica_log WHERE seq>0");

    // A missing trigger fails closed; a disabled log is refused.
    exec(&w.db, "DROP TRIGGER replica_messages_i");
    assert_eq!(
        pull(&w, "r1", None, &none).unwrap_err(),
        Refusal::Unavailable
    );
    let (_d, plain) = {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Connection::open(dir.path().join("p.sqlite3")).unwrap();
        crate::community::migrate_database(&mut db).unwrap();
        (dir, db)
    };
    let request = Request {
        recipient: "r1",
        replica_id: "rep-1",
        after: None,
        previous_eligible: &none,
        now_ms: 1,
    };
    assert_eq!(create(&plain, &request).unwrap_err(), Refusal::Disabled);
}

#[test]
fn a_replica_applies_only_what_is_exactly_for_it_and_keeps_its_store_on_failure() {
    let w = world();
    let export = full(&w);
    let server = crate::community::server_id(&w.db).unwrap();

    // Binding: another member, replica, source, format, a promotion claim.
    for mutate in [
        (|e: &mut Export| e.manifest.recipient_user_id = "x1".into()) as fn(&mut Export),
        |e| e.manifest.replica_id = "other".into(),
        |e| e.manifest.source_server_id = "elsewhere".into(),
        |e| e.manifest.replica_format = 2,
        |e| e.manifest.schema_version += 1,
        |e| e.manifest.promotion_eligible = true,
    ] {
        let mut store = store_for(&w, "r1");
        let mut tampered = export.clone();
        mutate(&mut tampered);
        assert_eq!(store.apply(&tampered, 1).unwrap_err(), ApplyError::Binding);
        assert!(store.cursor().unwrap().is_none());
    }
    // A store for another member refuses an export meant for r1.
    let mut other = Store::new(
        Connection::open_in_memory().unwrap(),
        &server,
        "x1",
        "rep-1",
    )
    .unwrap();
    assert_eq!(other.apply(&export, 1).unwrap_err(), ApplyError::Binding);

    // Integrity: a changed cell, a missing row, an extra table, a nested value
    // (even with a digest the attacker recomputes), a changed eligible list.
    let mut cases: Vec<Export> = Vec::new();
    let mut changed = export.clone();
    changed.tables.get_mut("messages").unwrap().rows[0][0] = json!("tampered");
    cases.push(changed);
    let mut missing = export.clone();
    missing.tables.get_mut("messages").unwrap().rows.pop();
    cases.push(missing);
    let mut extra = export.clone();
    extra.tables.insert(
        "users; DROP TABLE users".into(),
        extra.tables["users"].clone(),
    );
    cases.push(extra);
    let mut renamed = export.clone();
    let users = renamed.tables.remove("users").unwrap();
    renamed
        .tables
        .insert("users; DROP TABLE users".into(), users);
    cases.push(renamed);
    let mut nested = export.clone();
    nested.tables.get_mut("categories").unwrap().rows.push(vec![
        json!(["x"]),
        json!("n"),
        json!(0),
    ]);
    let d = digest(&nested.tables["categories"]);
    nested.manifest.tables.insert("categories".into(), d);
    cases.push(nested);
    let mut eligible = export.clone();
    eligible.manifest.eligible.push("priv".into());
    cases.push(eligible);
    let mut renamed_column = export.clone();
    renamed_column.tables.get_mut("users").unwrap().columns[1] = "password_hash".into();
    cases.push(renamed_column);
    let mut tombstones = export.clone();
    tombstones
        .tables
        .get_mut("users")
        .unwrap()
        .tombstones
        .push(vec![json!("m1")]);
    let d = digest(&tombstones.tables["users"]);
    tombstones.manifest.tables.insert("users".into(), d);
    cases.push(tombstones);
    for (n, bad) in cases.iter().enumerate() {
        let mut store = store_for(&w, "r1");
        assert_eq!(
            store.apply(bad, 1).unwrap_err(),
            ApplyError::Integrity,
            "case {n}"
        );
        assert!(
            store.cursor().unwrap().is_none(),
            "case {n}: nothing was stored"
        );
        assert!(
            store.dump().values().all(|rows| rows.is_empty()),
            "case {n}"
        );
    }

    // A failed delta leaves the earlier store exactly as it was.
    let mut store = store_for(&w, "r1");
    store.apply(&export, 1).unwrap();
    let before = store.dump();
    exec(&w.db, "INSERT INTO categories VALUES('A','x',0)");
    let cursor = store.cursor().unwrap().unwrap();
    let mut delta = pull(&w, "r1", Some(cursor.head), &cursor.eligible).unwrap();
    delta.tables.get_mut("categories").unwrap().rows[0][1] = json!("tampered");
    assert_eq!(store.apply(&delta, 2).unwrap_err(), ApplyError::Integrity);
    assert_eq!(store.dump(), before);

    // Cursor rules: wrong start, other lineage, no store yet, lower term.
    let good = pull(&w, "r1", Some(cursor.head), &cursor.eligible).unwrap();
    let mut wrong_start = good.clone();
    wrong_start.manifest.after = Some(cursor.head + 1);
    assert_eq!(
        store.apply(&wrong_start, 3).unwrap_err(),
        ApplyError::Cursor
    );
    let mut lineage = good.clone();
    lineage.manifest.lineage = "another".into();
    assert_eq!(store.apply(&lineage, 3).unwrap_err(), ApplyError::Lineage);
    assert_eq!(
        store_for(&w, "r1").apply(&good, 3).unwrap_err(),
        ApplyError::Lineage
    );
    exec(&w.db, "UPDATE replica_state SET term=5");
    let high = full(&w);
    store.apply(&high, 4).unwrap();
    exec(&w.db, "UPDATE replica_state SET term=4");
    assert_eq!(
        store.apply(&full(&w), 5).unwrap_err(),
        ApplyError::StaleTerm
    );
    // The good delta still applies on a store at the right place, and a new
    // lineage (restore or re-enable) forces a full snapshot.
    let mut fresh = store_for(&w, "r1");
    fresh.apply(&high, 6).unwrap();
    exec(&w.db, "UPDATE replica_state SET term=5");
    enable(&w.db).unwrap();
    disable(&w.db).unwrap();
    enable(&w.db).unwrap();
    let c = fresh.cursor().unwrap().unwrap();
    let delta = pull(&w, "r1", Some(c.head), &c.eligible).unwrap();
    assert_eq!(
        fresh.apply(&delta, 7).unwrap_err(),
        ApplyError::Lineage,
        "a new lineage needs a full snapshot"
    );
    fresh.apply(&full(&w), 8).unwrap();
}

#[test]
fn files_are_listed_fetched_and_checked_one_by_one() {
    let w = world();
    let export = full(&w);
    let listed: Vec<_> = export
        .manifest
        .files
        .iter()
        .map(|f| (f.table.as_str(), f.id.as_str(), f.version, f.size))
        .collect();
    assert_eq!(
        listed,
        [("uploads", "up_pub", 0, 7), ("user_profiles", "m1", 2, 12)],
        "a file of an unreadable message is not even listed"
    );
    let mut store = store_for(&w, "r1");
    let applied = store.apply(&export, 1).unwrap();
    assert_eq!(applied.files_pending, 2);
    assert_eq!(store.files_pending().unwrap().len(), 2);

    let file = export.manifest.files[0].clone();
    let got = read_file(&w.db, "r1", &file).unwrap();
    assert_eq!(got.bytes, b"PUBFILE");
    // A wrong hash or size is refused and nothing is stored.
    assert_eq!(
        store.accept_file(&file, "00", &got.bytes).unwrap_err(),
        ApplyError::File
    );
    assert_eq!(
        store
            .accept_file(&file, &got.sha256, b"PUBFIL")
            .unwrap_err(),
        ApplyError::File
    );
    store.accept_file(&file, &got.sha256, &got.bytes).unwrap();
    assert_eq!(store.files_pending().unwrap().len(), 1);
    // Restarting a partial download needs a fresh full manifest. A verified,
    // unchanged object must survive it, otherwise large copies never finish.
    store.apply(&full(&w), 2).unwrap();
    assert_eq!(
        store.files_pending().unwrap(),
        vec![export.manifest.files[1].clone()]
    );
    // The avatar too.
    let avatar = export.manifest.files[1].clone();
    let got = read_file(&w.db, "r1", &avatar).unwrap();
    store.accept_file(&avatar, &got.sha256, &got.bytes).unwrap();
    assert!(store.files_pending().unwrap().is_empty());

    // A file of a message the member cannot read, or from another member, is refused.
    let secret = FileRef {
        table: "uploads".into(),
        id: "up_priv".into(),
        version: 0,
        size: 10,
    };
    assert_eq!(
        read_file(&w.db, "r1", &secret).unwrap_err(),
        FileRefusal::Forbidden
    );
    assert_eq!(
        read_file(&w.db, "x1", &file).unwrap_err(),
        FileRefusal::Forbidden
    );
    // Changed between manifest and fetch: refused, never served as another version.
    exec(
        &w.db,
        "UPDATE uploads SET data=CAST('CHANGED!' AS BLOB),size=8 WHERE id='up_pub'",
    );
    assert_eq!(
        read_file(&w.db, "r1", &file).unwrap_err(),
        FileRefusal::Changed
    );
    exec(
        &w.db,
        "UPDATE user_profiles SET version=3,avatar=x'00' WHERE user_id='m1'",
    );
    assert_eq!(
        read_file(&w.db, "r1", &avatar).unwrap_err(),
        FileRefusal::Changed
    );
    exec(&w.db, "DELETE FROM uploads WHERE id='up_pub'");
    assert_eq!(
        read_file(&w.db, "r1", &file).unwrap_err(),
        FileRefusal::NotFound
    );
    assert_eq!(
        read_file(
            &w.db,
            "r1",
            &FileRef {
                table: "users".into(),
                id: "m1".into(),
                version: 0,
                size: 1
            }
        )
        .unwrap_err(),
        FileRefusal::NotFound
    );

    // The next delta brings the new version and drops the stale file entry.
    let c = store.cursor().unwrap().unwrap();
    let delta = pull(&w, "r1", Some(c.head), &c.eligible).unwrap();
    assert_eq!(delta.manifest.files.len(), 1);
    store.apply(&delta, 2).unwrap();
    let pending = store.files_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        (pending[0].table.as_str(), pending[0].version),
        ("user_profiles", 3)
    );
}

#[test]
fn a_full_refresh_keeps_only_verified_matching_files_in_the_same_lineage() {
    let w = world();
    let first = full(&w);
    let mut store = store_for(&w, "r1");
    store.apply(&first, 1).unwrap();
    for file in &first.manifest.files {
        let data = read_file(&w.db, "r1", file).unwrap();
        store.accept_file(file, &data.sha256, &data.bytes).unwrap();
    }
    store.apply(&full(&w), 2).unwrap();
    assert!(store.files_pending().unwrap().is_empty());
    exec(&w.db, "DELETE FROM uploads WHERE id='up_pub'; UPDATE user_profiles SET version=3,avatar=x'01' WHERE user_id='m1'");
    let refreshed = full(&w);
    store.apply(&refreshed, 3).unwrap();
    assert_eq!(store.files_pending().unwrap(), refreshed.manifest.files);
    let objects: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM replica_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        objects, 1,
        "the removed upload must not survive as an orphan"
    );
    let avatar = &refreshed.manifest.files[0];
    let data = read_file(&w.db, "r1", avatar).unwrap();
    store
        .accept_file(avatar, &data.sha256, &data.bytes)
        .unwrap();
    disable(&w.db).unwrap();
    enable(&w.db).unwrap();
    let restored = full(&w);
    assert_ne!(restored.manifest.lineage, refreshed.manifest.lineage);
    store.apply(&restored, 4).unwrap();
    assert_eq!(
        store.files_pending().unwrap(),
        restored.manifest.files,
        "a new lineage must not reuse a blob solely because its version matches"
    );
}

// ---------------------------------------------------------------------------
// The read-time bound: a long SQL read is cancelled on the export's own connection.

fn export_reader(w: &World) -> Connection {
    let path = w.db.path().expect("a file database").to_owned();
    let reader = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    reader
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    reader.pragma_update(None, "query_only", true).unwrap();
    reader
}

fn interrupted(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

#[test]
fn a_long_read_is_cancelled_on_the_exports_own_connection_and_never_touches_the_live_one() {
    let w = world();
    let reader = export_reader(&w);
    let guard = ReadDeadline::arm(&reader, std::time::Duration::from_millis(150))
        .expect("the mode of the connection is readable")
        .expect("armed on a read-only connection");
    // A real statement that would never end by itself.
    let started = std::time::Instant::now();
    let long = std::thread::spawn(move || {
        let result = reader.query_row(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT count(*) FROM c",
            [],
            |r| r.get::<_, i64>(0),
        );
        (reader, result)
    });
    // The live database keeps working the whole time, reads and writes alike.
    let mut writes = 0;
    while started.elapsed() < std::time::Duration::from_millis(400) {
        exec(
            &w.db,
            &format!("INSERT OR IGNORE INTO categories VALUES('live{writes}','x',0)"),
        );
        let count: i64 =
            w.db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))
                .unwrap();
        assert_eq!(count, 4);
        writes += 1;
    }
    let (reader, result) = long.join().unwrap();
    let error = result.expect_err("the long read must not complete");
    assert!(interrupted(&error), "{error:?}");
    assert!(guard.fired());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(writes > 0);
    // Only that statement was cut: the same reader connection works afterwards,
    // and so does the live one.
    assert_eq!(
        reader
            .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    exec(&w.db, "INSERT INTO categories VALUES('after','x',0)");
    // The cut-off maps to Busy, not to a damaged database.
    assert_eq!(Refusal::from(error), Refusal::Busy);
}

#[test]
fn a_read_write_connection_is_never_armed_so_the_live_database_is_never_interrupted() {
    let w = world();
    assert!(
        ReadDeadline::arm(&w.db, std::time::Duration::from_millis(10))
            .unwrap()
            .is_none()
    );
    // A long read on the live connection simply completes.
    let counted: i64 = w
        .db
        .query_row(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<1500000) SELECT count(*) FROM c",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(counted, 1_500_000);
    // And an export built on it takes the normal path and is not refused for time.
    assert!(create_with_deadline(
        &w.db,
        &Request {
            recipient: "r1",
            replica_id: "rep-1",
            after: None,
            previous_eligible: &BTreeSet::new(),
            now_ms: 1,
        },
        std::time::Duration::from_secs(60),
    )
    .is_ok());
}

#[test]
fn an_export_that_runs_past_its_deadline_is_refused_not_cut_short() {
    let w = world();
    // Enough ciphertext rows that a full export takes far longer than the tiny deadline.
    exec(
        &w.db,
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<80000)
         INSERT INTO messages(id,channel_id,author_id,content,created_at,created)
         SELECT 'bulk'||i,'pub','m1','relay-e2ee:v1:'||i,'t',i FROM n",
    );
    let none = BTreeSet::new();
    let request = Request {
        recipient: "r1",
        replica_id: "rep-1",
        after: None,
        previous_eligible: &none,
        now_ms: 1,
    };
    let reader = export_reader(&w);
    let started = std::time::Instant::now();
    let refused = create_with_deadline(&reader, &request, std::time::Duration::from_millis(30));
    assert_eq!(
        refused.unwrap_err(),
        Refusal::Busy,
        "never a smaller export"
    );
    let refused_after = started.elapsed();
    assert!(refused_after < std::time::Duration::from_secs(5));
    // The same data with the standard budget builds completely, so the refusal was the deadline's.
    let started = std::time::Instant::now();
    let whole =
        create_with_deadline(&reader, &request, std::time::Duration::from_secs(60)).unwrap();
    // The refusal came while the large table was still being read, not after it was done.
    assert!(
        refused_after < started.elapsed(),
        "{refused_after:?} vs {:?}",
        started.elapsed()
    );
    assert!(whole.tables["messages"].rows.len() >= 80_000);
    assert!(!whole.manifest.promotion_eligible);
    // The live database was not disturbed by either build.
    exec(
        &w.db,
        "INSERT INTO categories VALUES('still-writable','x',0)",
    );
}

// ---------------------------------------------------------------------------
// One shared budget while reading: too big is refused, never a prefix.

fn bulk_messages(w: &World, channel: &str, prefix: &str, count: usize, with_reactions: bool) {
    exec(
        &w.db,
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
             INSERT INTO messages(id,channel_id,author_id,content,created_at,created)
             SELECT '{prefix}'||i,'{channel}','m1','relay-e2ee:v1:'||i,'t',i FROM n;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
             INSERT INTO message_order(message_id) SELECT '{prefix}'||i FROM n;"
        ),
    );
    if with_reactions {
        exec(
            &w.db,
            &format!(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
                 INSERT INTO reactions SELECT '{prefix}'||i,'x','x1' FROM n;
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{count})
                 INSERT INTO reactions SELECT '{prefix}'||i,'y','x1' FROM n;"
            ),
        );
    }
}

fn build_with(
    w: &World,
    after: Option<i64>,
    previous: &BTreeSet<String>,
    limits: Limits,
) -> Result<Export, Refusal> {
    create_with_limits(
        &export_reader(w),
        &Request {
            recipient: "r1",
            replica_id: "rep-1",
            after,
            previous_eligible: previous,
            now_ms: 1_000,
        },
        std::time::Duration::from_secs(60),
        limits,
    )
}

fn limits(max_bytes: usize, max_keys: usize) -> Limits {
    Limits {
        max_bytes,
        max_keys,
    }
}

#[test]
fn a_full_snapshot_over_the_budget_is_refused_and_one_that_fits_is_complete() {
    let w = world();
    bulk_messages(&w, "pub", "bulk", 3_000, false);
    let none = BTreeSet::new();
    let whole = build_with(&w, None, &none, Limits::default()).unwrap();
    let needed: usize = whole
        .tables
        .values()
        .map(|t| serde_json::to_vec(t).unwrap().len())
        .sum();
    assert!(whole.tables["messages"].rows.len() >= 3_000);

    // Half of what it needs: refused. Never the half that fit.
    for budget in [needed / 2, needed / 10, 64] {
        assert_eq!(
            build_with(&w, None, &none, limits(budget, MAX_DELTA_KEYS)).unwrap_err(),
            Refusal::TooLarge,
            "budget {budget}"
        );
    }
    // With room, the same build is the same export (ids and times aside).
    let again = build_with(&w, None, &none, limits(needed * 16, MAX_DELTA_KEYS)).unwrap();
    assert_eq!(again.tables, whole.tables);
    assert_eq!(again.manifest.tables, whole.manifest.tables);
    assert_eq!(again.manifest.eligible, whole.manifest.eligible);

    // A bad row at the end would fail decoding if it were visited. The small
    // allowance must stop before that row, not collect the table then refuse.
    exec(
        &w.db,
        "INSERT INTO messages(id,channel_id,author_id,content,created_at,created)
        VALUES('zz-last-invalid','pub','m1',X'78','t',99)",
    );
    assert_eq!(
        build_with(&w, None, &none, limits(needed / 2, MAX_DELTA_KEYS)).unwrap_err(),
        Refusal::TooLarge
    );
    assert_eq!(
        build_with(&w, None, &none, Limits::default()).unwrap_err(),
        Refusal::Database
    );
}

#[test]
fn one_oversized_cell_is_refused_before_it_is_copied() {
    let w = world();
    exec(
        &w.db,
        "INSERT INTO messages(id,channel_id,author_id,content,created_at,created)
         VALUES('huge','pub','m1',printf('%.*c',400000,'x'),'t',99)",
    );
    let none = BTreeSet::new();
    assert_eq!(
        build_with(&w, None, &none, limits(100_000, MAX_DELTA_KEYS)).unwrap_err(),
        Refusal::TooLarge
    );
    assert!(build_with(&w, None, &none, limits(8 * 1024 * 1024, MAX_DELTA_KEYS)).is_ok());
}

#[test]
fn a_delta_that_touches_too_much_is_refused_while_its_keys_are_collected() {
    let w = world();
    let first = full(&w);
    let eligible: BTreeSet<String> = first.manifest.eligible.iter().cloned().collect();

    // Moved channel: a private channel with many messages becomes readable, so
    // every one of them has to be re-read. The key cap stops it while collecting.
    bulk_messages(&w, "priv", "moved", 2_000, false);
    let base = head(&w.db).unwrap();
    exec(
        &w.db,
        "UPDATE channels SET visibility='public' WHERE id='priv'",
    );
    assert_eq!(
        build_with(&w, Some(base), &eligible, limits(MAX_EXPORT_BYTES, 500)).unwrap_err(),
        Refusal::TooLarge,
        "gaining a big channel is a full snapshot, never a truncated delta"
    );
    let delta = build_with(&w, Some(base), &eligible, Limits::default()).unwrap();
    assert!(delta.tables["messages"].rows.len() >= 2_000);

    // A small byte budget refuses the same delta too.
    assert_eq!(
        build_with(&w, Some(base), &eligible, limits(20_000, MAX_DELTA_KEYS)).unwrap_err(),
        Refusal::TooLarge
    );
}

#[test]
fn dependency_expansion_counts_against_the_same_key_cap() {
    let w = world();
    let first = full(&w);
    let eligible: BTreeSet<String> = first.manifest.eligible.iter().cloned().collect();
    bulk_messages(&w, "pub", "dep", 1_000, true);
    let base = head(&w.db).unwrap();
    // 1 000 changed messages: each drags in its order slot and two reactions.
    exec(
        &w.db,
        "UPDATE messages SET content='relay-e2ee:v1:edited-'||id WHERE id LIKE 'dep%'",
    );
    // The log alone is 1 000 keys: under the cap. The children push it over.
    assert_eq!(
        build_with(&w, Some(base), &eligible, limits(MAX_EXPORT_BYTES, 1_500)).unwrap_err(),
        Refusal::TooLarge
    );
    let delta = build_with(&w, Some(base), &eligible, limits(MAX_EXPORT_BYTES, 4_500)).unwrap();
    assert_eq!(delta.tables["messages"].rows.len(), 1_000);
    assert_eq!(delta.tables["reactions"].rows.len(), 2_000);
    assert_eq!(delta.tables["message_order"].rows.len(), 1_000);
}

#[test]
fn staged_parts_borrow_the_export_and_reassemble_to_exactly_it() {
    let w = world();
    bulk_messages(&w, "pub", "part", 600, false);
    let export = full(&w);
    for limit in [2_048usize, 16 * 1024, 8 * 1024 * 1024] {
        let views = parts(&export, limit);
        assert!(!views.is_empty());
        let mut decoded = Vec::new();
        for view in &views {
            // The rows of a part are the export's own memory, not a copy of it.
            let data = &export.tables[view.table];
            if let Some(first) = view.rows.first() {
                let start = data.rows.as_ptr() as usize;
                let at = first as *const Vec<Value> as usize;
                assert!(
                    at >= start && at < start + data.rows.len() * std::mem::size_of::<Vec<Value>>(),
                    "part rows must point into the export"
                );
            }
            let bytes = serde_json::to_vec(view).unwrap();
            decoded.push(serde_json::from_slice::<PartData>(&bytes).unwrap());
        }
        let rebuilt = assemble(export.manifest.clone(), decoded).unwrap();
        assert_eq!(rebuilt.tables, export.tables, "limit {limit}");
    }
    assert!(
        parts(&export, 2_048).len() > parts(&export, 8 * 1024 * 1024).len(),
        "a small limit cuts the same data into more parts"
    );
}

#[test]
fn a_read_only_reader_whose_watchdog_cannot_start_builds_nothing() {
    let w = world();
    let reader = export_reader(&w);
    // Fails closed: no deadline means no read on a read-only connection.
    let refused = ReadDeadline::arm_with(&reader, std::time::Duration::from_secs(60), |_job| {
        Err(std::io::Error::other("thread limit reached"))
    });
    assert_eq!(refused.err(), Some(Refusal::Busy));
    // The live read-write connection is never armed, so it never needs a thread.
    let live = ReadDeadline::arm_with(&w.db, std::time::Duration::from_secs(60), |_job| {
        panic!("a read-write connection must not start a watchdog")
    });
    assert!(live.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Retention is a periodic pass, and the log stops with the last accepted Replica.

#[test]
fn the_retention_pass_prunes_by_the_gate_and_stops_the_log_when_no_replica_is_left() {
    let w = world();
    let now = 2_000_000_000;
    let count = |table: &str| -> i64 {
        w.db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    // Few rows: nothing is scanned or removed, however old.
    exec(&w.db, "UPDATE replica_log SET at_s=1");
    let before = count("replica_log");
    assert!(before > 0);
    assert_eq!(maintain(&w.db, now).unwrap(), Maintained::Kept);
    assert_eq!(count("replica_log"), before);

    // Far over MIN_ENTRIES with nothing recent: the pass cuts back to MIN_ENTRIES.
    exec(
        &w.db,
        &format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<{})
             INSERT INTO replica_log(tbl,op,pk,at_s) SELECT 'categories','i',json_array('x'||i),1 FROM n",
            MIN_ENTRIES + 500
        ),
    );
    let floor_before = state(&w.db).unwrap().floor;
    let rows_before = count("replica_log");
    // Failure between DELETE and the floor update must roll back the deletion.
    exec(
        &w.db,
        "CREATE TRIGGER qa_refuse_floor BEFORE UPDATE OF floor ON replica_state
        WHEN new.floor>old.floor BEGIN SELECT RAISE(ABORT,'injected floor failure'); END",
    );
    assert!(maintain(&w.db, now).is_err());
    assert_eq!(count("replica_log"), rows_before);
    assert_eq!(state(&w.db).unwrap().floor, floor_before);
    exec(&w.db, "DROP TRIGGER qa_refuse_floor");
    let Maintained::Pruned(removed) = maintain(&w.db, now).unwrap() else {
        panic!("expected a prune");
    };
    assert!(removed >= 500, "{removed}");
    assert_eq!(count("replica_log"), MIN_ENTRIES);
    assert!(
        state(&w.db).unwrap().floor > floor_before,
        "pruning raises the floor"
    );

    // The only accepted Replica leaves: the pass stops the log and clears everything.
    exec(
        &w.db,
        "INSERT INTO replica_tokens(user_id,token_hash,created,expires) VALUES('r1','h',1,9999999999)",
    );
    let lineage = state(&w.db).unwrap().lineage;
    exec(
        &w.db,
        "UPDATE membership SET status='left' WHERE user_id='r1'",
    );
    assert_eq!(maintain(&w.db, now).unwrap(), Maintained::Retired);
    let after = state(&w.db).unwrap();
    assert!(!after.enabled);
    assert_ne!(after.lineage, lineage, "every earlier copy must start over");
    assert_eq!(count("replica_log"), 0);
    assert_eq!(count("replica_tokens"), 0);
    // Nothing is recorded any more, and a second pass has nothing to do.
    exec(&w.db, "INSERT INTO categories VALUES('quiet','x',0)");
    assert_eq!(count("replica_log"), 0);
    assert_eq!(maintain(&w.db, now).unwrap(), Maintained::Off);
}
