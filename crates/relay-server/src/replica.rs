//! M2 slices 1 and 2: what a read-only copy may ever contain, and the ordered
//! change log that says which rows to re-read. Nothing here exports data,
//! opens a route, authenticates a replica or changes how the server answers a
//! request; the log is off until `enable` is called.
//!
//! The contract is `docs/m2-replica-contract-20261023.md` (revision 2) on the
//! feature branch. The table registry below is that contract's inventory as
//! data, and a test fails when a migration adds a table or column that is not
//! classified. The log is key-only (`table, op, primary key`), never holds row
//! contents or blobs, and is written by triggers in the same transaction as the
//! write it describes. Sequence numbers are ordered but not dense; nothing here
//! infers loss from a gap.
//!
//! The log starts only when a member who holds an accepted `Replica` role mints
//! their own pull token (`service::mint_token`, same transaction) and stops, and
//! is cleared, when no accepted `Replica` is left. Retention is a periodic cap
//! (`maintain`, from the cleanup loop), not a per-write limit.
use super::*;

pub(crate) mod export;
#[cfg(test)]
mod export_tests;
pub(crate) mod service;
#[cfg(test)]
mod service_tests;
pub(crate) mod store;
#[cfg(test)]
mod tests;

/// Cap on log rows; it wins over the 14-day window. It is a periodic cap: `maintain`
/// enforces it on the server's cleanup tick, not on every write, so between two
/// passes the log can hold more than this by what was written in the meantime.
pub(crate) const HARD_CAP: i64 = 1_000_000;
/// Rows kept even when older than the window.
pub(crate) const MIN_ENTRIES: i64 = 100_000;
pub(crate) const RETENTION_SECS: i64 = 14 * 24 * 60 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Exported for every recipient (listed columns).
    Export,
    /// Exported only for targets the recipient may access.
    Target,
    /// Ciphertext rows for every target; plaintext only for accessible ones.
    CiphertextWide,
    /// Exported exactly when the message it belongs to is.
    FollowsMessage,
    /// Not exported, and not eligible for promotion (contract 3.4).
    Withheld,
    /// Secret or capability: never exported, logged or reported (contract 3.5).
    Never,
    /// Short-lived state, rebuilt by use.
    Ephemeral,
    /// Rebuilt by the replica from what it may read.
    Derived,
}

impl Class {
    fn logged(self) -> bool {
        matches!(
            self,
            Self::Export | Self::Target | Self::CiphertextWide | Self::FollowsMessage
        )
    }
}

pub(crate) struct Table {
    pub name: &'static str,
    pub class: Class,
    /// Primary key columns, in declaration order. Empty for tables that are not logged.
    pub key: &'static [&'static str],
    /// Columns exported as they are.
    pub columns: &'static [&'static str],
    /// Columns exported only under the table's rule (accessible target, own group).
    pub conditional: &'static [&'static str],
    /// Columns never exported, or carried as file objects instead of inline.
    pub held_back: &'static [&'static str],
}

const fn t(
    name: &'static str,
    class: Class,
    key: &'static [&'static str],
    columns: &'static [&'static str],
    conditional: &'static [&'static str],
    held_back: &'static [&'static str],
) -> Table {
    Table {
        name,
        class,
        key,
        columns,
        conditional,
        held_back,
    }
}

/// Whole-table classes (no column detail needed).
const fn whole(name: &'static str, class: Class) -> Table {
    t(name, class, &[], &[], &[], &[])
}

use Class::*;

pub(crate) const TABLES: &[Table] = &[
    t(
        "users",
        Export,
        &["id"],
        &["id", "username", "role"],
        &[],
        &["password_hash"],
    ),
    t(
        "membership",
        Export,
        &["user_id"],
        &["user_id", "status"],
        &[],
        &[],
    ),
    t(
        "user_profiles",
        Export,
        &["user_id"],
        &["user_id", "display_name", "bio", "version", "appearance"],
        &[],
        &["avatar"],
    ),
    t(
        "channels",
        Target,
        &["id"],
        &[
            "id",
            "kind",
            "visibility",
            "category_id",
            "position",
            "voice_parent_id",
            "slowmode_seconds",
        ],
        &["name", "topic"],
        &[],
    ),
    t(
        "categories",
        Export,
        &["id"],
        &["id", "name", "position"],
        &[],
        &[],
    ),
    t(
        "channel_permissions",
        Target,
        &["channel_id", "subject_kind", "subject_id"],
        &["channel_id", "subject_kind", "subject_id", "permissions"],
        &[],
        &[],
    ),
    t(
        "community_roles",
        Export,
        &["id"],
        &["id", "name", "color", "position", "permissions"],
        &[],
        &[],
    ),
    t(
        "member_roles",
        Export,
        &["user_id", "role_id"],
        &["user_id", "role_id"],
        &[],
        &[],
    ),
    t(
        "conversations",
        Target,
        &["channel_id"],
        &["channel_id", "kind", "owner_id", "parent_id", "archived"],
        &[],
        &[],
    ),
    t(
        "conversation_members",
        Target,
        &["channel_id", "user_id"],
        &["channel_id", "user_id"],
        &[],
        &[],
    ),
    t(
        "messages",
        CiphertextWide,
        &["id"],
        &["id", "channel_id", "author_id", "created_at", "created"],
        &[
            "recipient_id",
            "content",
            "edited_at",
            "reply_to_id",
            "client_id",
            "request_hash",
        ],
        &[],
    ),
    t(
        "message_order",
        FollowsMessage,
        &["sequence"],
        &["sequence", "message_id"],
        &[],
        &[],
    ),
    t(
        "message_changes",
        FollowsMessage,
        &["sequence"],
        &[
            "sequence",
            "kind",
            "message_id",
            "channel_id",
            "recipient_id",
            "author_id",
            "deleted",
        ],
        &[],
        &[],
    ),
    t(
        "send_receipts",
        FollowsMessage,
        &["author_id", "client_id"],
        &["author_id", "client_id", "request_hash", "message_id"],
        &[],
        &[],
    ),
    t(
        "reactions",
        FollowsMessage,
        &["message_id", "emoji", "user_id"],
        &["message_id", "emoji", "user_id"],
        &[],
        &[],
    ),
    t(
        "mentions",
        FollowsMessage,
        &["message_id", "user_id"],
        &["message_id", "user_id"],
        &[],
        &[],
    ),
    t(
        "uploads",
        FollowsMessage,
        &["id"],
        &[
            "id",
            "owner_id",
            "message_id",
            "name",
            "mime",
            "size",
            "created",
        ],
        &[],
        &["data"],
    ),
    t(
        "channel_pins",
        Target,
        &["message_id"],
        &[
            "message_id",
            "channel_id",
            "pinned_by",
            "pinned_at",
            "pinned_ms",
        ],
        &[],
        &[],
    ),
    t(
        "read_states",
        Target,
        &["user_id", "target_kind", "target_id"],
        &["user_id", "target_kind", "target_id", "last_read_sequence"],
        &[],
        &[],
    ),
    t(
        "scheduled_events",
        Target,
        &["id"],
        &[
            "id",
            "name",
            "description",
            "channel_id",
            "starts_at",
            "ends_at",
        ],
        &[],
        &[],
    ),
    t(
        "event_interest",
        Target,
        &["event_id", "user_id"],
        &["event_id", "user_id"],
        &[],
        &[],
    ),
    t(
        "presentation_assets",
        Export,
        &["id"],
        &["id", "kind", "name", "owner_id", "version"],
        &[],
        &["data"],
    ),
    t(
        "community_system_events",
        Export,
        &["sequence"],
        &["sequence", "user_id", "kind", "created_at"],
        &[],
        &[],
    ),
    // Only rows of active devices of active members leave the source
    // (the predicate of `e2ee::directory`); inactive rows are withheld.
    t(
        "device_attestations",
        Export,
        &["device_id"],
        &["device_id", "user_id", "invite_id", "mac", "created"],
        &[],
        &[],
    ),
    t(
        "crypto_groups",
        Export,
        &["target_kind", "target_id"],
        &["target_kind", "target_id", "group_id", "sequence", "epoch"],
        &[],
        &[],
    ),
    t(
        "crypto_operations",
        Export,
        &["group_id", "operation_id"],
        &[
            "group_id",
            "operation_id",
            "author_user_id",
            "author_device_id",
            "request_hash",
        ],
        &[],
        &[],
    ),
    // Opaque member-supplied bytes. The body leaves the source only for groups
    // the recipient belongs to; the server does not parse it.
    t(
        "crypto_events",
        Export,
        &["group_id", "sequence"],
        &[
            "group_id",
            "sequence",
            "epoch",
            "kind",
            "author_user_id",
            "author_device_id",
            "recipient_device_id",
            "operation_id",
        ],
        &["body"],
        &[],
    ),
    t(
        "crypto_devices",
        Export,
        &["device_id"],
        &["device_id", "user_id", "identity", "active"],
        &[],
        &["key_package"],
    ),
    // Only an allow-list of keys (`METADATA_KEYS`) is exported or logged.
    t("metadata", Export, &["key"], &["key", "value"], &[], &[]),
    // Withheld: verifiers, personal choices and capabilities. Not eligible for promotion.
    whole("invites", Withheld),
    whole("member_invitations", Withheld),
    whole("linked_identities", Withheld),
    whole("friendships", Withheld),
    whole("user_blocks", Withheld),
    whole("group_invitations", Withheld),
    whole("host_locator_records", Withheld),
    whole("user_reports", Withheld),
    whole("audit_log", Withheld),
    whole("crypto_key_packages", Withheld),
    whole("crypto_welcome_packages", Withheld),
    whole("crypto_key_package_consumptions", Withheld),
    // Never: secrets and capabilities.
    whole("sessions", Never),
    whole("authenticator_credentials", Never),
    whole("authenticator_setup", Never),
    whole("authenticator_recovery", Never),
    whole("identity_link_challenges", Never),
    whole("identity_login_challenges", Never),
    whole("voice_resume_leases", Never),
    whole("private_call_recovery", Never),
    whole("replica_state", Never),
    whole("replica_log", Never),
    whole("replica_tokens", Never),
    whole("password_resets", Never),
    // Ephemeral or derived.
    whole("user_presence", Ephemeral),
    whole("stage_members", Ephemeral),
    whole("channel_send_cooldowns", Ephemeral),
    whole("message_send_log", Ephemeral),
    whole("crypto_call_requests", Ephemeral),
    whole("message_search", Derived),
    whole("schema_migrations", Derived),
];

/// `metadata` keys that are exported and logged. Everything else is host-bound
/// configuration or a secret (`open_call_secret`).
pub(crate) const METADATA_KEYS: &[&str] =
    &["server_id", "presentation", "protection", "hosting.v1", "upload_policy"];

/// FTS5 creates shadow tables named after the virtual table.
pub(crate) fn table_for(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|table| {
        table.name == name
            || (table.name == "message_search" && name.starts_with("message_search_"))
    })
}

fn logged_tables() -> impl Iterator<Item = &'static Table> {
    TABLES.iter().filter(|table| table.class.logged())
}

// ---------------------------------------------------------------------------
// Migration 24

const GATE: &str = "(SELECT enabled FROM replica_state WHERE id=1)=1";

/// The primary key as a JSON array, so the export can read it back exactly.
fn key_expr(table: &Table, row: &str) -> String {
    let columns = table
        .key
        .iter()
        .map(|column| format!("{row}.{column}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("json_array({columns})")
}

pub(crate) fn refresh_upload_metadata_triggers(tx: &Connection) -> rusqlite::Result<()> {
    for (name, sql) in trigger_definitions().into_iter().filter(|(name, _)| name.starts_with("replica_metadata_")) {
        tx.execute_batch(&format!("DROP TRIGGER IF EXISTS {name};{sql}"))?;
    }
    Ok(())
}

fn metadata_filter(row: &str) -> String {
    let keys = METADATA_KEYS
        .iter()
        .map(|key| format!("'{key}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{row}.key IN({keys})")
}

fn log_row(table: &Table, op: char, row: &str) -> String {
    format!(
        "INSERT INTO replica_log(tbl,op,pk,at_s) VALUES('{}','{op}',{},unixepoch());",
        table.name,
        key_expr(table, row)
    )
}

/// `(name, sql)` for every trigger this migration owns.
pub(crate) fn trigger_definitions() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for table in logged_tables() {
        let name = table.name;
        let (when_new, when_old, when_either) = if name == "metadata" {
            (
                format!(" AND {}", metadata_filter("new")),
                format!(" AND {}", metadata_filter("old")),
                format!(
                    " AND ({} OR {})",
                    metadata_filter("new"),
                    metadata_filter("old")
                ),
            )
        } else {
            (String::new(), String::new(), String::new())
        };
        out.push((
            format!("replica_{name}_i"),
            format!(
                "CREATE TRIGGER IF NOT EXISTS replica_{name}_i AFTER INSERT ON {name} WHEN {GATE}{when_new} BEGIN {} END",
                log_row(table, 'i', "new")
            ),
        ));
        // An update that changes the key leaves a tombstone for the old key.
        let changed = table
            .key
            .iter()
            .map(|column| format!("old.{column} IS NOT new.{column}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        out.push((
            format!("replica_{name}_u"),
            format!(
                "CREATE TRIGGER IF NOT EXISTS replica_{name}_u AFTER UPDATE ON {name} WHEN {GATE}{when_either} BEGIN {} INSERT INTO replica_log(tbl,op,pk,at_s) SELECT '{name}','d',{},unixepoch() WHERE {changed}; END",
                log_row(table, 'i', "new"),
                key_expr(table, "old")
            ),
        ));
        out.push((
            format!("replica_{name}_d"),
            format!(
                "CREATE TRIGGER IF NOT EXISTS replica_{name}_d AFTER DELETE ON {name} WHEN {GATE}{when_old} BEGIN {} END",
                log_row(table, 'd', "old")
            ),
        ));
    }
    // Any change to what decides who may read what bumps the permission generation.
    let bump = "UPDATE replica_state SET permission_generation=permission_generation+1 WHERE id=1;";
    for (table, update_of) in PERMISSION_TABLES {
        for (suffix, event) in [("i", "INSERT"), ("d", "DELETE")] {
            out.push((
                format!("replica_pg_{table}_{suffix}"),
                format!(
                    "CREATE TRIGGER IF NOT EXISTS replica_pg_{table}_{suffix} AFTER {event} ON {table} WHEN {GATE} BEGIN {bump} END"
                ),
            ));
        }
        let of = if update_of.is_empty() {
            String::new()
        } else {
            format!(" OF {update_of}")
        };
        out.push((
            format!("replica_pg_{table}_u"),
            format!(
                "CREATE TRIGGER IF NOT EXISTS replica_pg_{table}_u AFTER UPDATE{of} ON {table} WHEN {GATE} BEGIN {bump} END"
            ),
        ));
    }
    out
}

/// Tables whose changes can alter a recipient's access, with the columns that
/// matter on UPDATE (empty means any column).
pub(crate) const PERMISSION_TABLES: &[(&str, &str)] = &[
    ("channel_permissions", ""),
    ("community_roles", ""),
    ("member_roles", ""),
    ("conversations", ""),
    ("conversation_members", ""),
    ("membership", "status"),
    ("channels", "visibility,voice_parent_id,kind"),
    ("users", "role"),
];

/// Idempotent, like the other migrations: the legacy-migration tests re-run the
/// current step on a database that already has it.
pub(crate) fn migrate(tx: &Connection) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS replica_state(
            id INTEGER PRIMARY KEY CHECK(id=1),
            enabled INTEGER NOT NULL DEFAULT 0 CHECK(enabled IN(0,1)),
            lineage TEXT NOT NULL,
            term INTEGER NOT NULL DEFAULT 0 CHECK(term>=0),
            floor INTEGER NOT NULL DEFAULT 0 CHECK(floor>=0),
            permission_generation INTEGER NOT NULL DEFAULT 0 CHECK(permission_generation>=0),
            trigger_set_sha256 TEXT NOT NULL DEFAULT '',
            created_ms INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS replica_log(
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            tbl TEXT NOT NULL,
            op TEXT NOT NULL CHECK(op IN('i','d')),
            pk TEXT NOT NULL,
            at_s INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS replica_tokens(
            user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            token_hash TEXT NOT NULL UNIQUE,
            created INTEGER NOT NULL,
            expires INTEGER NOT NULL,
            last_used INTEGER,
            served_head INTEGER NOT NULL DEFAULT 0,
            served_bytes INTEGER NOT NULL DEFAULT 0,
            ack_head INTEGER,
            ack_ms INTEGER,
            claim_files_pending INTEGER,
            claim_ms INTEGER);",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO replica_state(id,lineage,created_ms) VALUES(1,?1,?2)",
        params![id(), now_ms() as i64],
    )?;
    for (_, sql) in trigger_definitions() {
        tx.execute_batch(&sql)?;
    }
    let digest = trigger_set_digest(tx)?;
    tx.execute(
        "UPDATE replica_state SET trigger_set_sha256=?1 WHERE id=1",
        [digest],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// State, enabling, integrity, retention

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub enabled: bool,
    pub lineage: String,
    pub term: i64,
    pub floor: i64,
    pub permission_generation: i64,
}

pub(crate) fn state(db: &Connection) -> rusqlite::Result<State> {
    db.query_row(
        "SELECT enabled,lineage,term,floor,permission_generation FROM replica_state WHERE id=1",
        [],
        |r| {
            Ok(State {
                enabled: r.get::<_, i64>(0)? == 1,
                lineage: r.get(1)?,
                term: r.get(2)?,
                floor: r.get(3)?,
                permission_generation: r.get(4)?,
            })
        },
    )
}

/// The newest sequence number ever assigned. It does not fall when old rows
/// are pruned (it reads the AUTOINCREMENT counter, not `MAX(seq)`).
pub(crate) fn head(db: &Connection) -> rusqlite::Result<i64> {
    Ok(db
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='replica_log'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Start recording changes. A new lineage and `floor = head` mean every copy
/// made before must take a full snapshot. Idempotent while enabled.
pub(crate) fn enable(db: &Connection) -> rusqlite::Result<()> {
    if state(db)?.enabled {
        return Ok(());
    }
    reset_lineage(db, true)
}

/// Stop recording. Anything that copied before must start over when enabled again.
pub(crate) fn disable(db: &Connection) -> rusqlite::Result<()> {
    reset_lineage(db, false)
}

fn reset_lineage(db: &Connection, enabled: bool) -> rusqlite::Result<()> {
    let floor = head(db)?;
    db.execute(
        "UPDATE replica_state SET enabled=?1,lineage=?2,floor=?3,created_ms=?4 WHERE id=1",
        params![enabled as i64, id(), floor, now_ms() as i64],
    )?;
    Ok(())
}

/// After `restore_database`: the restored file carries another history, so no
/// earlier cursor is valid. New lineage, `floor = head`, enabled flag kept.
pub(crate) fn after_restore(db: &Connection) -> rusqlite::Result<()> {
    // A restored file may revive a token that was revoked after the backup.
    db.execute("DELETE FROM replica_tokens", [])?;
    let enabled = state(db)?.enabled;
    reset_lineage(db, enabled)
}

/// SHA-256 over the name and SQL of every trigger this migration owns, in name order.
pub(crate) fn trigger_set_digest(db: &Connection) -> rusqlite::Result<String> {
    let mut statement = db.prepare(
        "SELECT name,sql FROM sqlite_master WHERE type='trigger' AND name LIKE 'replica\\_%' ESCAPE '\\' ORDER BY name",
    )?;
    let mut hasher = Sha256::new();
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (name, sql) = row?;
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(sql.as_bytes());
        hasher.update([0]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// True only if every trigger exists and none was altered since the migration.
/// A replica service must refuse to run when this is false.
pub(crate) fn trigger_set_intact(db: &Connection) -> rusqlite::Result<bool> {
    let stored: String = db.query_row(
        "SELECT trigger_set_sha256 FROM replica_state WHERE id=1",
        [],
        |r| r.get(0),
    )?;
    let count: usize = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'replica\\_%' ESCAPE '\\'",
        [],
        |r| r.get(0),
    )?;
    Ok(!stored.is_empty()
        && count == trigger_definitions().len()
        && trigger_set_digest(db)? == stored)
}

/// What one pass of `maintain` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Maintained {
    /// The log is off: nothing to keep.
    Off,
    /// The log is on and within retention (or too small to need a scan).
    Kept,
    /// Rows beyond retention were removed.
    Pruned(usize),
    /// No accepted Replica was left: the log was stopped and cleared.
    Retired,
}

/// Stop and clear the change log when no member holds an accepted `Replica` role.
/// Nothing needs the log then, and it must not keep recording what nobody copies.
/// A new lineage (from `disable`) means any copy made before must start over, and
/// no pull token survives. Returns whether it stopped anything.
pub(crate) fn retire_if_unused(db: &Connection) -> rusqlite::Result<bool> {
    if !state(db)?.enabled || crate::hosting::any_replica(db) {
        return Ok(false);
    }
    disable(db)?;
    db.execute("DELETE FROM replica_log", [])?;
    db.execute("DELETE FROM replica_tokens", [])?;
    Ok(true)
}

/// The retention hook, run periodically by the server's cleanup loop: stop the log
/// when nobody holds the role, otherwise enforce retention. It is cheap on the
/// common path: a state read, a roster read, and a subtraction (`head - floor`
/// bounds the rows left, so up to `MIN_ENTRIES` nothing is scanned or deleted).
pub(crate) fn maintain(db: &Connection, now_s: i64) -> rusqlite::Result<Maintained> {
    // Removing log entries and advancing the oldest valid cursor must commit
    // together. Otherwise a failed update/crash could hide missing changes from
    // a replica whose cursor still looks valid. Retirement is atomic here too.
    let tx = db.unchecked_transaction()?;
    let outcome = maintain_in_transaction(&tx, now_s)?;
    tx.commit()?;
    Ok(outcome)
}

fn maintain_in_transaction(db: &Connection, now_s: i64) -> rusqlite::Result<Maintained> {
    let current = state(db)?;
    if !current.enabled {
        return Ok(Maintained::Off);
    }
    if retire_if_unused(db)? {
        return Ok(Maintained::Retired);
    }
    if head(db)? - current.floor <= MIN_ENTRIES {
        return Ok(Maintained::Kept);
    }
    Ok(Maintained::Pruned(prune(db, now_s)?))
}

/// Drop log rows beyond retention. `keep = min(HARD_CAP, max(MIN_ENTRIES,
/// entries newer than 14 days))`: the cap wins over the window. Raises
/// `floor` to the newest pruned sequence. Returns the number of rows removed.
pub(crate) fn prune(db: &Connection, now_s: i64) -> rusqlite::Result<usize> {
    let recent: i64 = db.query_row(
        "SELECT COUNT(*) FROM replica_log WHERE at_s>=?1",
        [now_s - RETENTION_SECS],
        |r| r.get(0),
    )?;
    let keep = recent.clamp(MIN_ENTRIES, HARD_CAP);
    let cutoff: Option<i64> = db
        .query_row(
            "SELECT seq FROM replica_log ORDER BY seq DESC LIMIT 1 OFFSET ?1",
            [keep],
            |r| r.get(0),
        )
        .optional()?;
    let Some(cutoff) = cutoff else {
        return Ok(0);
    };
    let removed = db.execute("DELETE FROM replica_log WHERE seq<=?1", [cutoff])?;
    db.execute(
        "UPDATE replica_state SET floor=MAX(floor,?1) WHERE id=1",
        [cutoff],
    )?;
    Ok(removed)
}
