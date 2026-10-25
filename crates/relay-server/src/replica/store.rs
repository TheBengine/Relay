//! M2 slice 3, replica side, in process: a separate store that applies an
//! export only after it has checked who it is for, where it came from, that
//! nothing was cut or changed, and that it continues exactly where the store
//! stopped. Any failure keeps the previous store untouched.
//!
//! The store has its own tables, built from the same registry as the source
//! (no constraints, no foreign keys, no secrets). Table and column names are
//! taken from the registry, never from the export; values are bound, never
//! spliced into SQL. It never serves a client, signs anyone in or accepts an
//! invitation: it only holds a read-only copy and says how complete it is.
use super::export::{digest, eligible_digest, Export, FileRef, REPLICA_FORMAT};
use super::*;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplyError {
    /// Another recipient, replica, source, format or schema; or a promotion claim.
    Binding,
    /// The source's term is lower than one already seen.
    StaleTerm,
    /// A delta needs the same lineage; take a full snapshot.
    Lineage,
    /// A delta must start exactly at the store's head.
    Cursor,
    /// A count, digest or shape does not match the manifest.
    Integrity,
    /// A file does not match its manifest entry.
    File,
    Database,
}

impl From<rusqlite::Error> for ApplyError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Database
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub lineage: String,
    pub term: i64,
    pub head: i64,
    pub permission_generation: i64,
    pub eligible: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    pub rows: usize,
    pub tombstones: usize,
    pub files_pending: usize,
}

pub(crate) struct Store {
    db: Connection,
    source_server_id: String,
    recipient: String,
    replica_id: String,
}

fn cells(table: &Table) -> Vec<&'static str> {
    table
        .columns
        .iter()
        .chain(table.conditional)
        .copied()
        .collect()
}

impl Store {
    pub(crate) fn new(
        db: Connection,
        source_server_id: &str,
        recipient: &str,
        replica_id: &str,
    ) -> Result<Self, ApplyError> {
        for table in logged_tables() {
            db.execute_batch(&format!(
                "CREATE TABLE IF NOT EXISTS {}({},PRIMARY KEY({}));",
                table.name,
                cells(table).join(","),
                table.key.join(",")
            ))?;
        }
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS replica_cursor(id INTEGER PRIMARY KEY CHECK(id=1),lineage TEXT NOT NULL,term INTEGER NOT NULL,head INTEGER NOT NULL,permission_generation INTEGER NOT NULL,eligible TEXT NOT NULL,export_id TEXT NOT NULL,applied_ms INTEGER NOT NULL,promotion_eligible INTEGER NOT NULL CHECK(promotion_eligible=0));
             CREATE TABLE IF NOT EXISTS replica_files(tbl TEXT NOT NULL,id TEXT NOT NULL,version INTEGER NOT NULL,size INTEGER NOT NULL,sha256 TEXT,data BLOB,PRIMARY KEY(tbl,id,version));",
        )?;
        Ok(Self {
            db,
            source_server_id: source_server_id.into(),
            recipient: recipient.into(),
            replica_id: replica_id.into(),
        })
    }

    pub(crate) fn cursor(&self) -> Result<Option<Cursor>, ApplyError> {
        let row: Option<(String, i64, i64, i64, String)> = self
            .db
            .query_row(
                "SELECT lineage,term,head,permission_generation,eligible FROM replica_cursor WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        row.map(|(lineage, term, head, permission_generation, eligible)| {
            let eligible: Vec<String> =
                serde_json::from_str(&eligible).map_err(|_| ApplyError::Integrity)?;
            Ok(Cursor {
                lineage,
                term,
                head,
                permission_generation,
                eligible: eligible.into_iter().collect(),
            })
        })
        .transpose()
    }

    /// Files named by the manifests so far that have not arrived (or did not verify).
    pub(crate) fn files_pending(&self) -> Result<Vec<FileRef>, ApplyError> {
        let mut statement = self.db.prepare(
            "SELECT tbl,id,version,size FROM replica_files WHERE data IS NULL ORDER BY tbl,id",
        )?;
        let files = statement
            .query_map([], |r| {
                Ok(FileRef {
                    table: r.get(0)?,
                    id: r.get(1)?,
                    version: r.get(2)?,
                    size: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(files)
    }

    /// Match the listed size/version, then verify the digest returned over the
    /// authenticated source connection. The manifest itself carries no blob hash.
    pub(crate) fn accept_file(
        &mut self,
        file: &FileRef,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<(), ApplyError> {
        if bytes.len() as i64 != file.size || format!("{:x}", Sha256::digest(bytes)) != sha256 {
            return Err(ApplyError::File);
        }
        let known = self.db.execute(
            "UPDATE replica_files SET data=?4,sha256=?5 WHERE tbl=?1 AND id=?2 AND version=?3 AND size=?6",
            params![file.table, file.id, file.version, bytes, sha256, file.size],
        )?;
        if known == 1 {
            Ok(())
        } else {
            Err(ApplyError::File)
        }
    }

    fn check(&self, export: &Export, cursor: &Option<Cursor>) -> Result<(), ApplyError> {
        let m = &export.manifest;
        if m.replica_format != REPLICA_FORMAT
            || m.schema_version != crate::community::SCHEMA_VERSION
            || m.source_server_id != self.source_server_id
            || m.recipient_user_id != self.recipient
            || m.replica_id != self.replica_id
            || m.promotion_eligible
        {
            return Err(ApplyError::Binding);
        }
        if let Some(cursor) = cursor {
            if m.term < cursor.term {
                return Err(ApplyError::StaleTerm);
            }
            if m.lineage == cursor.lineage && m.head < cursor.head {
                return Err(ApplyError::Cursor);
            }
        }
        match m.after {
            None => {}
            Some(after) => {
                let Some(cursor) = cursor else {
                    return Err(ApplyError::Lineage);
                };
                if m.lineage != cursor.lineage {
                    return Err(ApplyError::Lineage);
                }
                if after != cursor.head || m.head < after {
                    return Err(ApplyError::Cursor);
                }
            }
        }
        // Shape: exactly the registry's tables and columns, scalar cells only.
        let expected: Vec<&Table> = logged_tables().collect();
        if export.tables.len() != expected.len() || m.tables.len() != expected.len() {
            return Err(ApplyError::Integrity);
        }
        for table in expected {
            let (Some(data), Some(claimed)) =
                (export.tables.get(table.name), m.tables.get(table.name))
            else {
                return Err(ApplyError::Integrity);
            };
            let columns = cells(table);
            if data.columns != columns {
                return Err(ApplyError::Integrity);
            }
            if m.after.is_none() && !data.tombstones.is_empty() {
                return Err(ApplyError::Integrity);
            }
            let scalar =
                |v: &Value| matches!(v, Value::Null | Value::String(_)) || v.as_i64().is_some();
            if data
                .rows
                .iter()
                .any(|row| row.len() != columns.len() || !row.iter().all(scalar))
                || data
                    .tombstones
                    .iter()
                    .any(|key| key.len() != table.key.len() || !key.iter().all(scalar))
                || digest(data) != *claimed
            {
                return Err(ApplyError::Integrity);
            }
        }
        if eligible_digest(&m.eligible) != m.eligible_set_sha256 {
            return Err(ApplyError::Integrity);
        }
        Ok(())
    }

    /// Apply one export in one transaction, or change nothing.
    pub(crate) fn apply(&mut self, export: &Export, now_ms: u64) -> Result<Applied, ApplyError> {
        let cursor = self.cursor()?;
        self.check(export, &cursor)?;
        let m = &export.manifest;
        let files: std::collections::BTreeMap<_, _> = m
            .files
            .iter()
            .map(|f| ((f.table.as_str(), f.id.as_str()), f))
            .collect();
        if files.len() != m.files.len() {
            return Err(ApplyError::Integrity);
        }
        let tx = self.db.transaction()?;
        if m.after.is_none() {
            for table in logged_tables() {
                tx.execute(&format!("DELETE FROM {}", table.name), [])?;
            }
            if !cursor
                .as_ref()
                .is_some_and(|c| c.lineage == m.lineage && c.term == m.term)
            {
                tx.execute("DELETE FROM replica_files", [])?;
            }
        }
        let (mut rows, mut tombstones) = (0, 0);
        for table in logged_tables() {
            let data = &export.tables[table.name];
            let key_at: Vec<usize> = table
                .key
                .iter()
                .map(|k| cells(table).iter().position(|c| c == k).unwrap_or_default())
                .collect();
            for key in &data.tombstones {
                let condition = table
                    .key
                    .iter()
                    .enumerate()
                    .map(|(i, k)| format!("{k}=?{}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                tx.execute(
                    &format!("DELETE FROM {} WHERE {condition}", table.name),
                    rusqlite::params_from_iter(bound(key)?),
                )?;
                tombstones += 1;
                if is_file_table(table.name) {
                    if let Some(id) = key.first().and_then(Value::as_str) {
                        tx.execute(
                            "DELETE FROM replica_files WHERE tbl=?1 AND id=?2",
                            params![table.name, id],
                        )?;
                    }
                }
            }
            let marks = (1..=data.columns.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            for row in &data.rows {
                tx.execute(
                    &format!(
                        "INSERT OR REPLACE INTO {}({}) VALUES({marks})",
                        table.name,
                        data.columns.join(",")
                    ),
                    rusqlite::params_from_iter(bound(row)?),
                )?;
                rows += 1;
                if is_file_table(table.name) {
                    // A re-sent row replaces what was known about its file, unless
                    // the manifest names the same version and size.
                    if let Some(id) = key_at.first().and_then(|i| row[*i].as_str()) {
                        match files.get(&(table.name, id)) {
                            Some(keep) => tx.execute(
                                "DELETE FROM replica_files WHERE tbl=?1 AND id=?2 AND NOT(version=?3 AND size=?4)",
                                params![table.name, id, keep.version, keep.size],
                            )?,
                            None => tx.execute(
                                "DELETE FROM replica_files WHERE tbl=?1 AND id=?2",
                                params![table.name, id],
                            )?,
                        };
                    }
                }
            }
        }
        for file in &m.files {
            tx.execute(
                "INSERT OR IGNORE INTO replica_files(tbl,id,version,size) VALUES(?1,?2,?3,?4)",
                params![file.table, file.id, file.version, file.size],
            )?;
        }
        if m.after.is_none() {
            // A fresh full manifest may follow an interrupted file download.
            // Preserve verified matching objects in the same lineage, but never
            // retain an orphan or an object whose version/size changed.
            tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS copy_wanted_files(tbl TEXT,id TEXT,version INTEGER,size INTEGER,PRIMARY KEY(tbl,id)); DELETE FROM copy_wanted_files;")?;
            for file in &m.files {
                tx.execute(
                    "INSERT INTO copy_wanted_files VALUES(?1,?2,?3,?4)",
                    params![file.table, file.id, file.version, file.size],
                )?;
            }
            tx.execute("DELETE FROM replica_files WHERE NOT EXISTS(SELECT 1 FROM copy_wanted_files w WHERE w.tbl=replica_files.tbl AND w.id=replica_files.id AND w.version=replica_files.version AND w.size=replica_files.size)",[])?;
            tx.execute("DELETE FROM copy_wanted_files", [])?;
        }
        let eligible = serde_json::to_string(&m.eligible).map_err(|_| ApplyError::Integrity)?;
        tx.execute(
            "INSERT OR REPLACE INTO replica_cursor(id,lineage,term,head,permission_generation,eligible,export_id,applied_ms,promotion_eligible) VALUES(1,?1,?2,?3,?4,?5,?6,?7,0)",
            params![m.lineage, m.term, m.head, m.permission_generation, eligible, m.export_id, now_ms as i64],
        )?;
        let pending: i64 = tx.query_row(
            "SELECT COUNT(*) FROM replica_files WHERE data IS NULL",
            [],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(Applied {
            rows,
            tombstones,
            files_pending: pending as usize,
        })
    }

    /// Every stored row, for comparing two stores in tests and for reports.
    #[cfg(test)]
    pub(crate) fn dump(&self) -> std::collections::BTreeMap<String, Vec<Vec<Value>>> {
        use rusqlite::types::ValueRef;
        let mut out = std::collections::BTreeMap::new();
        for table in logged_tables() {
            let width = cells(table).len();
            let mut statement = self
                .db
                .prepare(&format!(
                    "SELECT {} FROM {} ORDER BY {}",
                    cells(table).join(","),
                    table.name,
                    table.key.join(",")
                ))
                .unwrap();
            let rows = statement
                .query_map([], |r| {
                    Ok((0..width)
                        .map(|i| match r.get_ref(i).unwrap() {
                            ValueRef::Null => Value::Null,
                            ValueRef::Integer(n) => Value::from(n),
                            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into()),
                            _ => panic!("unexpected cell"),
                        })
                        .collect::<Vec<_>>())
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            out.insert(table.name.to_string(), rows);
        }
        out
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.db
    }
}

fn is_file_table(name: &str) -> bool {
    matches!(name, "uploads" | "user_profiles" | "presentation_assets")
}

fn bound(values: &[Value]) -> Result<Vec<rusqlite::types::Value>, ApplyError> {
    values
        .iter()
        .map(|v| super::export::sql_value(v).ok_or(ApplyError::Integrity))
        .collect()
}
