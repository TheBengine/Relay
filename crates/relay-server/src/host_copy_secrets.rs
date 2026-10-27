//! Login secrets stay on the machine that issued them.
//!
//! A full community copy (planned handover image, or the database a co-host
//! promotes after an unexpected outage) carries members, channels, roles,
//! history and files. It never carries password hashes, session verifiers,
//! second-factor secrets, login challenges, voice resume leases or data-copy
//! tokens. After a move every member signs in again on the new host with the
//! same account; nothing a stolen copy holds can be used or cracked offline.
use rusqlite::{Connection, OptionalExtension};

/// Stored instead of a password hash. It is not a valid PHC string, so password
/// sign-in fails until the member signs in with their Relay account or an
/// administrator sets a new password.
pub(crate) const NOT_COPIED: &str = "!password-not-copied-to-this-host";

/// Tables the M2 member copy never exports, but a full standby copy keeps,
/// because the new host needs them and they hold no login secret.
#[cfg(test)]
pub(crate) const KEPT_IN_FULL_COPY: &[(&str, &str)] = &[
    (
        "private_call_recovery",
        "accepted private calls resume after the move; holds call state, no token",
    ),
    (
        "replica_state",
        "member data copies keep their lineage and continue after the move",
    ),
    (
        "replica_log",
        "member data copies catch up from the same change log",
    ),
    (
        "password_resets",
        "digests of random reset and recovery codes: the way back in after a move",
    ),
];

/// Children before parents, so the result is the same with or without
/// foreign key enforcement on the copy's connection.
const SECRET_TABLES: &[&str] = &[
    "voice_resume_leases",
    "authenticator_setup",
    "identity_link_challenges",
    "authenticator_recovery",
    "authenticator_credentials",
    "identity_login_challenges",
    "replica_tokens",
    "sessions",
];

fn exists(db: &Connection, table: &str) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
}

/// Removes every login secret from a database COPY and rewrites the file so the
/// deleted bytes do not survive in free pages. Never call this on the live
/// community database.
pub(crate) fn strip(db: &Connection) -> rusqlite::Result<()> {
    db.pragma_update(None, "secure_delete", true)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        for table in SECRET_TABLES {
            if exists(db, table)? {
                db.execute(&format!("DELETE FROM {table}"), [])?;
            }
        }
        db.execute("UPDATE users SET password_hash=?1", [NOT_COPIED])?;
        Ok(())
    })();
    match result {
        Ok(()) => db.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = db.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    db.execute_batch("VACUUM")
}

/// True when a copy holds no login secret.
pub(crate) fn is_stripped(db: &Connection) -> rusqlite::Result<bool> {
    for table in SECRET_TABLES {
        if exists(db, table)? {
            let any: bool =
                db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
                    r.get(0)
                })?;
            if any {
                return Ok(false);
            }
        }
    }
    let kept: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE password_hash!=?1)",
        [NOT_COPIED],
        |r| r.get(0),
    )?;
    Ok(!kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn community() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("copy.sqlite3");
        let mut db = Connection::open(&path).unwrap();
        crate::community::migrate_database(&mut db).unwrap();
        db.execute_batch(
            "INSERT INTO users VALUES('u1','anna','$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNoaGFzaA','owner');
             INSERT INTO sessions(token_hash,user_id,expires) VALUES('session-verifier-secret','u1',9999999999);
             INSERT INTO channels(id,name,kind,topic) VALUES('c1','general','text','');
             INSERT INTO invites(code_hash,expires,remaining_uses,label) VALUES('open-invite-hash',9999999999,3,'friends'),('spent-invite-hash',9999999999,0,'used up');
             INSERT INTO member_invitations(user_id,invite_id) VALUES('u1','used-invite-id');
             INSERT INTO authenticator_credentials(user_id,sealed,salt,revision) VALUES('u1',x'5345414c4544544f5450','salt','r1');
             INSERT INTO authenticator_recovery VALUES('u1','recovery-code-hash');",
        )
        .unwrap();
        (dir, path)
    }

    /// The complete standby contract: every table of the schema is either kept
    /// whole or stripped, and nothing the member copy calls a secret is kept by
    /// accident. A new table fails here until someone classifies it.
    #[test]
    fn every_table_of_a_full_copy_is_either_kept_or_stripped() {
        use crate::replica::{Class, TABLES};
        let (_dir, path) = community();
        let db = Connection::open(&path).unwrap();
        let mut stmt = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for secret in SECRET_TABLES {
            assert!(tables.iter().any(|t| t == secret), "{secret} vanished from the schema");
        }
        for table in TABLES.iter().filter(|t| t.class == Class::Never) {
            let stripped = SECRET_TABLES.contains(&table.name);
            let kept = KEPT_IN_FULL_COPY.iter().any(|(name, _)| *name == table.name);
            assert!(
                stripped ^ kept,
                "{} must be either stripped or kept with a reason",
                table.name
            );
        }
        for name in &tables {
            assert!(
                crate::replica::table_for(name).is_some(),
                "{name} is not in the data inventory"
            );
        }
    }

    #[test]
    fn invitations_protocol_state_and_versions_survive_the_strip() {
        let (_dir, path) = community();
        let db = Connection::open(&path).unwrap();
        let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        let migrations: i64 = db
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        strip(&db).unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0)).unwrap(),
            version
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            migrations
        );
        // Open and used-up invitations with their remaining uses, and the record
        // of which invitation a member used.
        assert_eq!(
            db.query_row("SELECT group_concat(remaining_uses) FROM (SELECT remaining_uses FROM invites ORDER BY label)", [], |r| r.get::<_, String>(0)).unwrap(),
            "3,0"
        );
        assert_eq!(
            db.query_row("SELECT invite_id FROM member_invitations", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "used-invite-id"
        );
        assert_eq!(
            db.query_row("SELECT value FROM metadata WHERE key='server_id'", [], |r| r.get::<_, String>(0))
                .unwrap()
                .len(),
            36
        );
    }

    #[test]
    fn a_full_copy_keeps_members_and_channels_but_no_password_hash_session_or_second_factor() {
        let (_dir, path) = community();
        let db = Connection::open(&path).unwrap();
        assert!(!is_stripped(&db).unwrap());
        strip(&db).unwrap();
        assert!(is_stripped(&db).unwrap());
        let (user, hash): (String, String) = db
            .query_row("SELECT username,password_hash FROM users", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(user, "anna");
        assert_eq!(hash, NOT_COPIED);
        assert!(argon2::PasswordHash::new(&hash).is_err());
        let channels: i64 = db
            .query_row("SELECT COUNT(*) FROM channels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(channels, 1);
        drop(db);
        // The removed bytes are gone from the file itself, not only unlinked.
        let raw = std::fs::read(&path).unwrap();
        for secret in [
            &b"session-verifier-secret"[..],
            b"$argon2id$",
            b"SEALEDTOTP",
            b"recovery-code-hash",
        ] {
            assert!(
                !raw.windows(secret.len()).any(|w| w == secret),
                "secret bytes survived in the copy"
            );
        }
    }
}
