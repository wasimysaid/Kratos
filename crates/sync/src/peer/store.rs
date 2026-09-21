use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use sha2::Digest as _;
use kratos_doc::{RegistryRow, RowOp, apply_op};

pub const MAX_ROW_BYTES: usize = 1024 * 1024;
pub const MAX_CHAT_ROWS: u64 = 4096;
pub const MAX_CHAT_ROW_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SIDECAR_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_TOOL_BLOB_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PeerStoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid data: {0}")]
    Invalid(&'static str),
}

#[derive(Clone)]
pub struct PeerStore {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) db: Mutex<Connection>,
    pub(crate) path: PathBuf,
    pub(crate) live: super::LiveState,
}

#[derive(Debug, Clone)]
pub(crate) struct ChatRow {
    pub seq: u64,
    pub device: String,
    pub batch_id: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChatStats {
    pub head_seq: u64,
    pub seq_floor: u64,
    pub checkpoint_seq: u64,
    pub checkpoint_size: u64,
    pub row_count: u64,
    pub row_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct ChatAppend {
    pub seq: u64,
    pub dup: bool,
}

#[derive(Debug)]
pub(crate) struct RegistryApply {
    pub batch: String,
    pub seq: u64,
    pub applied: usize,
    pub rows: Vec<RegistryRow>,
}

impl PeerStore {
    /// Open one durable peer database. All logical data is additionally keyed
    /// by authenticated profile, so separate local profiles cannot collide.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PeerStoreError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            inner: Arc::new(Inner {
                db: Mutex::new(conn),
                path,
                live: super::LiveState::default(),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Create a consistent standalone SQLite backup without constructing SQL
    /// from a profile or path. The temporary file is fsynced and atomically
    /// renamed over the requested destination.
    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<(), PeerStoreError> {
        let destination = destination.as_ref();
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = destination.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        {
            let db = self.db();
            db.execute("VACUUM INTO ?1", params![tmp.to_string_lossy().as_ref()])?;
        }
        // FlushFileBuffers requires a handle opened with write access on Windows.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tmp)?;
        file.sync_all()?;
        drop(file);
        let previous = destination.with_extension(format!("previous-{}", uuid::Uuid::new_v4()));
        let had_previous = destination.exists();
        if had_previous {
            std::fs::rename(destination, &previous)?;
        }
        if let Err(error) = std::fs::rename(&tmp, destination) {
            if had_previous {
                let _ = std::fs::rename(&previous, destination);
            }
            return Err(error.into());
        }
        if had_previous {
            std::fs::remove_file(previous)?;
        }
        #[cfg(unix)]
        if let Some(parent) = destination.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Close every active HTTP-upgraded stream authenticated as this device.
    /// Persisted data and queued nudges remain untouched.
    pub fn revoke_device(&self, profile: &str, device: &str) {
        self.inner.live.revoke(profile, device);
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Connection> {
        self.inner.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn chat_stats(
        &self,
        profile: &str,
        chat: &str,
    ) -> Result<ChatStats, PeerStoreError> {
        let db = self.db();
        let (head, floor, cp_seq, cp_size): (u64, u64, u64, u64) = db
            .query_row(
                "SELECT head_seq, seq_floor, checkpoint_seq, checkpoint_size FROM chat_meta WHERE profile=?1 AND chat=?2",
                params![profile, chat],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .unwrap_or((0, 0, 0, 0));
        let (count, bytes): (u64, u64) = db.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(bytes)),0) FROM chat_rows WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(ChatStats {
            head_seq: head,
            seq_floor: floor,
            checkpoint_seq: cp_seq,
            checkpoint_size: cp_size,
            row_count: count,
            row_bytes: bytes,
        })
    }

    pub(crate) fn chat_frontier(
        &self,
        profile: &str,
        chat: &str,
    ) -> Result<Vec<u8>, PeerStoreError> {
        let db = self.db();
        Ok(db
            .query_row(
                "SELECT frontier FROM chat_checkpoints WHERE profile=?1 AND chat=?2",
                params![profile, chat],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub(crate) fn chat_rows(
        &self,
        profile: &str,
        chat: &str,
        after: u64,
        exclude: Option<&str>,
    ) -> Result<Vec<ChatRow>, PeerStoreError> {
        let db = self.db();
        let mut out = Vec::new();
        if let Some(device) = exclude {
            let mut q = db.prepare("SELECT seq,device,batch_id,bytes FROM chat_rows WHERE profile=?1 AND chat=?2 AND seq>?3 AND device!=?4 ORDER BY seq")?;
            let rows = q.query_map(params![profile, chat, after, device], chat_row)?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let mut q = db.prepare("SELECT seq,device,batch_id,bytes FROM chat_rows WHERE profile=?1 AND chat=?2 AND seq>?3 ORDER BY seq")?;
            let rows = q.query_map(params![profile, chat, after], chat_row)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    pub(crate) fn append_chat(
        &self,
        profile: &str,
        chat: &str,
        device: &str,
        batch: &str,
        bytes: &[u8],
    ) -> Result<ChatAppend, PeerStoreError> {
        if bytes.is_empty() {
            return Err(PeerStoreError::Invalid("empty"));
        }
        if bytes.len() > MAX_ROW_BYTES {
            return Err(PeerStoreError::Invalid("too_large"));
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        let digest = format!("{:x}", sha2::Sha256::digest(bytes));
        if let Some((seq, original_device, original_digest)) = tx
            .query_row(
                "SELECT seq,device,digest FROM chat_batches WHERE profile=?1 AND chat=?2 AND batch_id=?3",
                params![profile, chat, batch],
                |r| Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
            )
            .optional()?
        {
            if original_device != device || original_digest != digest {
                return Err(PeerStoreError::Invalid("batch_conflict"));
            }
            return Ok(ChatAppend { seq, dup: true });
        }
        ensure_chat(&tx, profile, chat)?;

        // Catch-up is a single finite response in both HTTP and websocket
        // protocols. Keep the uncompacted log physically bounded rather than
        // silently truncating a replay and reporting an unreachable head.
        let (row_count, row_bytes): (u64, u64) = tx.query_row(
            "SELECT COUNT(*),COALESCE(SUM(LENGTH(bytes)),0) FROM chat_rows WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if row_count >= MAX_CHAT_ROWS || row_bytes + bytes.len() as u64 > MAX_CHAT_ROW_BYTES {
            return Err(PeerStoreError::Invalid("backlog_full"));
        }
        let head: u64 = tx.query_row(
            "SELECT head_seq FROM chat_meta WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| r.get(0),
        )?;
        let seq = head + 1;
        tx.execute("INSERT INTO chat_rows(profile,chat,seq,device,batch_id,bytes,received_at) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![profile, chat, seq, device, batch, bytes, now_ms()])?;

        tx.execute("INSERT INTO chat_batches(profile,chat,batch_id,device,digest,seq,received_at) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![profile, chat, batch, device, digest, seq, now_ms()])?;
        tx.execute(
            "UPDATE chat_meta SET head_seq=?3 WHERE profile=?1 AND chat=?2",
            params![profile, chat, seq],
        )?;
        tx.commit()?;
        Ok(ChatAppend { seq, dup: false })
    }

    pub(crate) fn checkpoint(
        &self,
        profile: &str,
        chat: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PeerStoreError> {
        let db = self.db();
        Ok(db
            .query_row(
                "SELECT seq_covered,bytes FROM chat_checkpoints WHERE profile=?1 AND chat=?2",
                params![profile, chat],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub(crate) fn commit_checkpoint(
        &self,
        profile: &str,
        chat: &str,
        seq: u64,
        frontier: &[u8],
        bytes: &[u8],
    ) -> Result<(u64, u64), PeerStoreError> {
        if bytes.is_empty() {
            return Err(PeerStoreError::Invalid("empty"));
        }
        if bytes.len() > MAX_CHECKPOINT_BYTES {
            return Err(PeerStoreError::Invalid("too_large"));
        }
        let advertised = loro::VersionVector::decode(frontier)
            .map_err(|_| PeerStoreError::Invalid("bad_frontier"))?;
        if seq > 0 && advertised.is_empty() {
            return Err(PeerStoreError::Invalid("bad_frontier"));
        }
        let checkpoint_doc = loro::LoroDoc::new();
        let checkpoint_status = checkpoint_doc
            .import(bytes)
            .map_err(|_| PeerStoreError::Invalid("bad_checkpoint"))?;
        if checkpoint_status.pending.is_some() {
            return Err(PeerStoreError::Invalid("bad_checkpoint"));
        }
        let checkpoint_vv = checkpoint_doc.oplog_vv();
        if !checkpoint_vv.includes_vv(&advertised) || !advertised.includes_vv(&checkpoint_vv) {
            return Err(PeerStoreError::Invalid("frontier_mismatch"));
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        ensure_chat(&tx, profile, chat)?;
        let (head, floor): (u64, u64) = tx.query_row(
            "SELECT head_seq,seq_floor FROM chat_meta WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if seq < floor {
            return Err(PeerStoreError::Invalid("floor_regression"));
        }
        if seq > head {
            return Err(PeerStoreError::Invalid("ahead_of_head"));
        }

        let materialized = loro::LoroDoc::new();
        if let Some((old_frontier, old_bytes)) = tx
            .query_row(
                "SELECT frontier,bytes FROM chat_checkpoints WHERE profile=?1 AND chat=?2",
                params![profile, chat],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            let old = loro::VersionVector::decode(&old_frontier)
                .map_err(|_| PeerStoreError::Invalid("stored_frontier_invalid"))?;
            if !advertised.includes_vv(&old) {
                return Err(PeerStoreError::Invalid("frontier_regression"));
            }
            let status = materialized
                .import(&old_bytes)
                .map_err(|_| PeerStoreError::Invalid("stored_checkpoint_invalid"))?;
            let actual = materialized.oplog_vv();
            if status.pending.is_some() || !actual.includes_vv(&old) || !old.includes_vv(&actual) {
                return Err(PeerStoreError::Invalid("stored_checkpoint_invalid"));
            }
        }

        let mut covered = tx.prepare(
            "SELECT bytes FROM chat_rows WHERE profile=?1 AND chat=?2 AND seq<=?3 ORDER BY seq",
        )?;
        let updates = covered
            .query_map(params![profile, chat, seq], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(covered);
        let status = materialized
            .import_batch(&updates)
            .map_err(|_| PeerStoreError::Invalid("bad_covered_row"))?;
        if status.pending.is_some() || !advertised.includes_vv(&materialized.oplog_vv()) {
            return Err(PeerStoreError::Invalid("checkpoint_missing_rows"));
        }
        let before: u64 = tx.query_row(
            "SELECT COUNT(*) FROM chat_rows WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| r.get(0),
        )?;
        tx.execute("INSERT INTO chat_checkpoints(profile,chat,seq_covered,frontier,bytes,committed_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(profile,chat) DO UPDATE SET seq_covered=excluded.seq_covered,frontier=excluded.frontier,bytes=excluded.bytes,committed_at=excluded.committed_at", params![profile,chat,seq,frontier,bytes,now_ms()])?;
        tx.execute(
            "DELETE FROM chat_rows WHERE profile=?1 AND chat=?2 AND seq<=?3",
            params![profile, chat, seq],
        )?;
        tx.execute("UPDATE chat_meta SET seq_floor=?3,checkpoint_seq=?3,checkpoint_size=?4 WHERE profile=?1 AND chat=?2", params![profile,chat,seq,bytes.len() as u64])?;
        let after: u64 = tx.query_row(
            "SELECT COUNT(*) FROM chat_rows WHERE profile=?1 AND chat=?2",
            params![profile, chat],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok((seq, before - after))
    }

    pub(crate) fn put_sidecar(
        &self,
        profile: &str,
        scope: &str,
        owner: &str,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<(), PeerStoreError> {
        let db = self.db();
        db.execute("INSERT INTO sidecars(profile,scope,owner,name,content_type,bytes,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(profile,scope,owner,name) DO UPDATE SET content_type=excluded.content_type,bytes=excluded.bytes,updated_at=excluded.updated_at", params![profile,scope,owner,name,content_type,bytes,now_ms()])?;
        Ok(())
    }

    pub(crate) fn get_sidecar(
        &self,
        profile: &str,
        scope: &str,
        owner: &str,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>, PeerStoreError> {
        let db = self.db();
        Ok(db.query_row("SELECT content_type,bytes FROM sidecars WHERE profile=?1 AND scope=?2 AND owner=?3 AND name=?4", params![profile,scope,owner,name], |r| Ok((r.get(0)?,r.get(1)?))).optional()?)
    }

    pub(crate) fn registry_state(
        &self,
        profile: &str,
        org: &str,
        cursor: Option<u64>,
    ) -> Result<(u64, bool, u64, Vec<RegistryRow>), PeerStoreError> {
        let db = self.db();
        let (seq, gc): (u64, u64) = db
            .query_row(
                "SELECT seq,gc_floor FROM registry_meta WHERE profile=?1 AND org=?2",
                params![profile, org],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        let full = cursor.is_none_or(|c| c < gc || c > seq);
        let since = if full { 0 } else { cursor.unwrap_or(0) };
        let mut q=db.prepare("SELECT row_json FROM registry_rows WHERE profile=?1 AND org=?2 AND seq>?3 ORDER BY seq,kind,id")?;
        let rows = q.query_map(params![profile, org, since], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(
                serde_json::from_str(&row?)
                    .map_err(|_| PeerStoreError::Invalid("corrupt_registry_row"))?,
            );
        }
        Ok((seq, full, gc, out))
    }

    pub(crate) fn apply_registry(
        &self,
        profile: &str,
        org: &str,
        batch: &str,
        ops: &[RowOp],
    ) -> Result<RegistryApply, PeerStoreError> {
        let mut db = self.db();
        let tx = db.transaction()?;
        if let Some((seq, applied)) = tx
            .query_row(
                "SELECT seq,applied FROM registry_batches WHERE profile=?1 AND org=?2 AND batch=?3",
                params![profile, org, batch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
        {
            return Ok(RegistryApply {
                batch: batch.into(),
                seq,
                applied,
                rows: Vec::new(),
            });
        }
        tx.execute("INSERT INTO registry_meta(profile,org,seq,gc_floor) VALUES(?1,?2,0,0) ON CONFLICT DO NOTHING",params![profile,org])?;
        let old_seq: u64 = tx.query_row(
            "SELECT seq FROM registry_meta WHERE profile=?1 AND org=?2",
            params![profile, org],
            |r| r.get(0),
        )?;
        let next = old_seq + 1;
        let mut changed = Vec::new();
        for op in ops {
            let prior:Option<String>=tx.query_row("SELECT row_json FROM registry_rows WHERE profile=?1 AND org=?2 AND kind=?3 AND id=?4",params![profile,org,op.kind,op.id],|r|r.get(0)).optional()?;
            let prior = prior
                .as_deref()
                .map(serde_json::from_str::<RegistryRow>)
                .transpose()
                .map_err(|_| PeerStoreError::Invalid("corrupt_registry_row"))?;
            let (row, did) = apply_op(prior.as_ref(), op);
            if did {
                let mut row = row.ok_or(PeerStoreError::Invalid("missing_row"))?;
                row.seq = next;
                let json = serde_json::to_string(&row)
                    .map_err(|_| PeerStoreError::Invalid("bad_registry_row"))?;
                tx.execute("INSERT INTO registry_rows(profile,org,kind,id,seq,row_json) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(profile,org,kind,id) DO UPDATE SET seq=excluded.seq,row_json=excluded.row_json",params![profile,org,row.kind,row.id,next,json])?;
                changed.push(row);
            }
        }
        let seq = if changed.is_empty() { old_seq } else { next };
        if !changed.is_empty() {
            tx.execute(
                "UPDATE registry_meta SET seq=?3 WHERE profile=?1 AND org=?2",
                params![profile, org, seq],
            )?;
        }
        tx.execute("INSERT INTO registry_batches(profile,org,batch,seq,applied,received_at) VALUES(?1,?2,?3,?4,?5,?6)",params![profile,org,batch,seq,changed.len(),now_ms()])?;
        tx.commit()?;
        Ok(RegistryApply {
            batch: batch.into(),
            seq,
            applied: changed.len(),
            rows: changed,
        })
    }

    pub(crate) fn queue_nudge(
        &self,
        profile: &str,
        device: &str,
        chat: &str,
    ) -> Result<(), PeerStoreError> {
        let db = self.db();
        db.execute("INSERT INTO nudges(profile,device,chat,queued_at) VALUES(?1,?2,?3,?4) ON CONFLICT(profile,device,chat) DO UPDATE SET queued_at=excluded.queued_at",params![profile,device,chat,now_ms()])?;
        db.execute("DELETE FROM nudges WHERE profile=?1 AND device=?2 AND chat NOT IN (SELECT chat FROM nudges WHERE profile=?1 AND device=?2 ORDER BY queued_at DESC LIMIT 256)",params![profile,device])?;
        Ok(())
    }
    pub(crate) fn pending_nudges(
        &self,
        profile: &str,
        device: &str,
    ) -> Result<Vec<String>, PeerStoreError> {
        let db = self.db();
        let mut q = db
            .prepare("SELECT chat FROM nudges WHERE profile=?1 AND device=?2 ORDER BY queued_at")?;
        Ok(q.query_map(params![profile, device], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }
    pub(crate) fn ack_nudge(
        &self,
        profile: &str,
        device: &str,
        chat: &str,
    ) -> Result<(), PeerStoreError> {
        self.db().execute(
            "DELETE FROM nudges WHERE profile=?1 AND device=?2 AND chat=?3",
            params![profile, device, chat],
        )?;
        Ok(())
    }
}

fn chat_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ChatRow> {
    Ok(ChatRow {
        seq: r.get(0)?,
        device: r.get(1)?,
        batch_id: r.get(2)?,
        bytes: r.get(3)?,
    })
}
fn ensure_chat(tx: &Transaction<'_>, profile: &str, chat: &str) -> rusqlite::Result<()> {
    tx.execute("INSERT INTO chat_meta(profile,chat,head_seq,seq_floor,checkpoint_seq,checkpoint_size) VALUES(?1,?2,0,0,0,0) ON CONFLICT DO NOTHING",params![profile,chat])?;
    Ok(())
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chat_meta(profile TEXT NOT NULL,chat TEXT NOT NULL,head_seq INTEGER NOT NULL,seq_floor INTEGER NOT NULL,checkpoint_seq INTEGER NOT NULL,checkpoint_size INTEGER NOT NULL,PRIMARY KEY(profile,chat));
CREATE TABLE IF NOT EXISTS chat_rows(profile TEXT NOT NULL,chat TEXT NOT NULL,seq INTEGER NOT NULL,device TEXT NOT NULL,batch_id TEXT NOT NULL,bytes BLOB NOT NULL,received_at INTEGER NOT NULL,PRIMARY KEY(profile,chat,seq),UNIQUE(profile,chat,batch_id));

CREATE TABLE IF NOT EXISTS chat_batches(profile TEXT NOT NULL,chat TEXT NOT NULL,batch_id TEXT NOT NULL,device TEXT NOT NULL,digest TEXT NOT NULL,seq INTEGER NOT NULL,received_at INTEGER NOT NULL,PRIMARY KEY(profile,chat,batch_id));
CREATE TABLE IF NOT EXISTS chat_checkpoints(profile TEXT NOT NULL,chat TEXT NOT NULL,seq_covered INTEGER NOT NULL,frontier BLOB NOT NULL,bytes BLOB NOT NULL,committed_at INTEGER NOT NULL,PRIMARY KEY(profile,chat));
CREATE TABLE IF NOT EXISTS registry_meta(profile TEXT NOT NULL,org TEXT NOT NULL,seq INTEGER NOT NULL,gc_floor INTEGER NOT NULL,PRIMARY KEY(profile,org));
CREATE TABLE IF NOT EXISTS registry_rows(profile TEXT NOT NULL,org TEXT NOT NULL,kind TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,row_json TEXT NOT NULL,PRIMARY KEY(profile,org,kind,id));
CREATE INDEX IF NOT EXISTS registry_rows_seq ON registry_rows(profile,org,seq);
CREATE TABLE IF NOT EXISTS registry_batches(profile TEXT NOT NULL,org TEXT NOT NULL,batch TEXT NOT NULL,seq INTEGER NOT NULL,applied INTEGER NOT NULL,received_at INTEGER NOT NULL,PRIMARY KEY(profile,org,batch));
CREATE TABLE IF NOT EXISTS nudges(profile TEXT NOT NULL,device TEXT NOT NULL,chat TEXT NOT NULL,queued_at INTEGER NOT NULL,PRIMARY KEY(profile,device,chat));
CREATE TABLE IF NOT EXISTS sidecars(profile TEXT NOT NULL,scope TEXT NOT NULL,owner TEXT NOT NULL,name TEXT NOT NULL,content_type TEXT NOT NULL,bytes BLOB NOT NULL,updated_at INTEGER NOT NULL,PRIMARY KEY(profile,scope,owner,name));
CREATE TABLE IF NOT EXISTS attachment_uploads(profile TEXT NOT NULL,upload TEXT NOT NULL,sender TEXT NOT NULL,target TEXT NOT NULL,file_name TEXT NOT NULL,length INTEGER NOT NULL,digest TEXT NOT NULL,committed INTEGER NOT NULL,created_at INTEGER NOT NULL,committed_at INTEGER,PRIMARY KEY(profile,upload,sender,target));
CREATE TABLE IF NOT EXISTS attachment_chunks(profile TEXT NOT NULL,upload TEXT NOT NULL,sender TEXT NOT NULL,target TEXT NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(profile,upload,sender,target,offset),FOREIGN KEY(profile,upload,sender,target) REFERENCES attachment_uploads(profile,upload,sender,target) ON DELETE CASCADE);

"#;
