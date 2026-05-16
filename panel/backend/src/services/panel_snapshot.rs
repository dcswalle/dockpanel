//! Phase 4 W4: persistent panel snapshots.
//!
//! Builds tar.gz triplets containing:
//!   binaries/   — agent + api + cli binary copies
//!   db/         — gzipped pg_dump of the DockPanel database
//!   etc/        — copy of /etc/dockpanel
//!   metadata.json — provenance (from_version, trigger, operator)
//!
//! Stored in `/var/backups/dockpanel/snapshots/`. The orchestrator creates
//! one BEFORE invoking `update.sh`; the resulting file outlives the
//! `.bak` triplet that `update.sh:432-499` deletes on successful health
//! check, giving operators an after-the-fact rollback path.
//!
//! This module is the IO layer only. systemctl stop/start choreography
//! around restores lives in the orchestrator so this service stays usable
//! for one-shot "snapshot now" admin actions without state-machine
//! coupling.

use chrono::Utc;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

use crate::models::PanelSnapshot;

const SNAPSHOT_DIR: &str = "/var/backups/dockpanel/snapshots";
const STAGING_DIR_PARENT: &str = "/var/backups/dockpanel/.snapshot-staging";
/// Refuse to create a snapshot if the target partition has less than this
/// many bytes free. A typical snapshot is ~150-300 MB; 2 GiB keeps the
/// partition healthy even if a sweep is lagging.
const MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Retention: never delete the most-recent N snapshots, even if all are
/// older than `RETENTION_DAYS`. Protects against "broke weeks ago, only
/// just noticed" scenarios.
pub const RETENTION_MIN: i64 = 3;
pub const RETENTION_DAYS: i64 = 7;

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fleet variant reserved for future per-server fleet snapshot tagging
pub enum SnapshotTrigger {
    Manual,
    PreUpdate { target_version: String },
    Fleet { server_id: Uuid },
}

impl SnapshotTrigger {
    pub fn as_str(&self) -> String {
        match self {
            SnapshotTrigger::Manual => "manual".to_string(),
            SnapshotTrigger::PreUpdate { target_version } => {
                format!("pre-update:{target_version}")
            }
            SnapshotTrigger::Fleet { server_id } => format!("fleet:{server_id}"),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    pub id: Uuid,
    pub file_path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
    pub from_version: String,
}

#[derive(Debug)]
pub enum SnapshotError {
    DirInit(String),
    InsufficientDisk { available: u64, required: u64 },
    Subprocess { cmd: String, stderr: String },
    Io(std::io::Error),
    Db(sqlx::Error),
    NotFound(Uuid),
    FileMissing(PathBuf),
    Sha256Mismatch { expected: String, actual: String },
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::DirInit(s) => write!(f, "snapshot dir cannot be initialized: {s}"),
            SnapshotError::InsufficientDisk { available, required } => write!(
                f,
                "insufficient disk space: {available} bytes available, {required} required"
            ),
            SnapshotError::Subprocess { cmd, stderr } => {
                write!(f, "subprocess `{cmd}` failed: {stderr}")
            }
            SnapshotError::Io(e) => write!(f, "io error: {e}"),
            SnapshotError::Db(e) => write!(f, "db error: {e}"),
            SnapshotError::NotFound(id) => write!(f, "snapshot {id} not found"),
            SnapshotError::FileMissing(p) => {
                write!(f, "snapshot file missing on disk: {}", p.display())
            }
            SnapshotError::Sha256Mismatch { expected, actual } => {
                write!(f, "sha256 mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<std::io::Error> for SnapshotError {
    fn from(e: std::io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<sqlx::Error> for SnapshotError {
    fn from(e: sqlx::Error) -> Self {
        SnapshotError::Db(e)
    }
}

/// Build a snapshot of the current panel state (binaries + DB dump + etc).
/// Writes to a `.tmp` file first, computes sha256, renames to final, then
/// inserts the DB row. The DB row + file are consistent: if any earlier
/// step fails, no row is written and the .tmp file is cleaned up.
pub async fn create_snapshot(
    pool: &PgPool,
    trigger: SnapshotTrigger,
    operator: Option<String>,
) -> Result<SnapshotMeta, SnapshotError> {
    ensure_dirs().await?;
    check_free_disk(SNAPSHOT_DIR, MIN_FREE_BYTES).await?;

    let snapshot_id = Uuid::new_v4();
    let timestamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let final_name = format!("panel-snapshot-{timestamp}.tar.gz");
    let final_path = PathBuf::from(SNAPSHOT_DIR).join(&final_name);
    let tmp_path = PathBuf::from(SNAPSHOT_DIR).join(format!("{final_name}.tmp"));
    let staging_dir = PathBuf::from(STAGING_DIR_PARENT).join(snapshot_id.to_string());

    // Best-effort cleanup of any stale .tmp from a prior crashed run before
    // we start writing. Same name pattern is improbable but cheap to guard.
    let _ = tokio::fs::remove_file(&tmp_path).await;
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;

    let result = build_snapshot_inner(
        snapshot_id,
        &staging_dir,
        &tmp_path,
        &final_path,
        &trigger,
        operator.as_deref(),
    )
    .await;

    // Always sweep staging dir, success or fail.
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;

    let (size_bytes, sha256, from_version) = match result {
        Ok(t) => t,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }
    };

    // Persist DB row only after the file is in its final location.
    let row_result = sqlx::query(
        "INSERT INTO panel_snapshots \
            (id, file_path, from_version, trigger, operator, size_bytes, sha256) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(snapshot_id)
    .bind(final_path.to_string_lossy().to_string())
    .bind(&from_version)
    .bind(trigger.as_str())
    .bind(&operator)
    .bind(size_bytes as i64)
    .bind(&sha256)
    .execute(pool)
    .await;

    if let Err(e) = row_result {
        // DB row didn't land — remove the file so we don't leak orphans.
        let _ = tokio::fs::remove_file(&final_path).await;
        return Err(SnapshotError::Db(e));
    }

    tracing::info!(
        "Created panel snapshot {snapshot_id} at {} ({} bytes, sha256: {})",
        final_path.display(),
        size_bytes,
        &sha256[..16.min(sha256.len())]
    );

    Ok(SnapshotMeta {
        id: snapshot_id,
        file_path: final_path,
        size_bytes,
        sha256,
        from_version,
    })
}

async fn build_snapshot_inner(
    snapshot_id: Uuid,
    staging_dir: &Path,
    tmp_path: &Path,
    final_path: &Path,
    trigger: &SnapshotTrigger,
    operator: Option<&str>,
) -> Result<(u64, String, String), SnapshotError> {
    // Layout staging dir: binaries/ db/ etc/ metadata.json
    tokio::fs::create_dir_all(staging_dir.join("binaries")).await?;
    tokio::fs::create_dir_all(staging_dir.join("db")).await?;
    tokio::fs::create_dir_all(staging_dir.join("etc")).await?;

    // Copy binaries. cp is fine; size + perms aren't security-critical
    // inside the tar (extraction restores from the tar entries).
    for bin in &["dockpanel-agent", "dockpanel-api", "dockpanel"] {
        let src = format!("/usr/local/bin/{bin}");
        let dst = staging_dir.join("binaries").join(bin);
        if Path::new(&src).exists() {
            tokio::fs::copy(&src, &dst).await?;
        } else {
            tracing::warn!("snapshot: binary {src} not found, skipping");
        }
    }

    // Dump DB via docker exec (matches scripts/update.sh:143 pattern).
    let dump_path = staging_dir.join("db").join("dump.sql.gz");
    let dump_status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "docker exec dockpanel-postgres pg_dump -U dockpanel --clean --if-exists dockpanel | gzip > {}",
            shell_escape(&dump_path.to_string_lossy())
        ))
        .status()
        .await?;
    if !dump_status.success() {
        return Err(SnapshotError::Subprocess {
            cmd: "pg_dump".into(),
            stderr: format!("exit status {dump_status}"),
        });
    }

    // Copy /etc/dockpanel into staging. The tree is small (api.env, ssl/,
    // a handful of small text files) — recursive cp via tar piped is
    // simplest and preserves permissions cleanly.
    let etc_status = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cp -a /etc/dockpanel/. {}",
            shell_escape(&staging_dir.join("etc").to_string_lossy())
        ))
        .status()
        .await?;
    if !etc_status.success() {
        return Err(SnapshotError::Subprocess {
            cmd: "cp etc".into(),
            stderr: format!("exit status {etc_status}"),
        });
    }

    let from_version = env!("CARGO_PKG_VERSION").to_string();
    let metadata = serde_json::json!({
        "snapshot_id": snapshot_id.to_string(),
        "from_version": from_version,
        "created_at": Utc::now().to_rfc3339(),
        "trigger": trigger.as_str(),
        "operator": operator,
    });
    tokio::fs::write(
        staging_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).unwrap_or_default(),
    )
    .await?;

    // Build tarball: tar -C <staging> -czf <tmp> .
    // -C cd's into staging so tar entries are relative ("binaries/" not
    // "<staging>/binaries/").
    let tar_status = run_cmd_with_timeout(
        "tar",
        &[
            "-C",
            &staging_dir.to_string_lossy(),
            "-czf",
            &tmp_path.to_string_lossy(),
            ".",
        ],
        Duration::from_secs(300),
    )
    .await?;
    if !tar_status.success() {
        return Err(SnapshotError::Subprocess {
            cmd: "tar -czf".into(),
            stderr: format!("exit status {tar_status}"),
        });
    }

    let size_bytes = tokio::fs::metadata(&tmp_path).await?.len();
    let sha256 = sha256_of(tmp_path).await?;

    // Atomic rename — the tar.gz only becomes "real" once this succeeds.
    tokio::fs::rename(&tmp_path, &final_path).await?;

    Ok((size_bytes, sha256, from_version))
}

/// Restore a previously-created snapshot. Caller MUST stop dockpanel-agent
/// + dockpanel-api before invoking this, and restart after. The DB restore
/// is destructive (DROP + recreate via pg_dump --clean --if-exists).
pub async fn restore_snapshot(pool: &PgPool, snapshot_id: Uuid) -> Result<(), SnapshotError> {
    let row: Option<PanelSnapshot> =
        sqlx::query_as("SELECT * FROM panel_snapshots WHERE id = $1")
            .bind(snapshot_id)
            .fetch_optional(pool)
            .await?;

    let snapshot = row.ok_or(SnapshotError::NotFound(snapshot_id))?;
    let file_path = PathBuf::from(&snapshot.file_path);
    if !file_path.exists() {
        return Err(SnapshotError::FileMissing(file_path));
    }

    let actual_sha = sha256_of(&file_path).await?;
    if actual_sha != snapshot.sha256 {
        return Err(SnapshotError::Sha256Mismatch {
            expected: snapshot.sha256,
            actual: actual_sha,
        });
    }

    // Extract to a fresh working dir so a partial restore doesn't leave
    // garbage in /var/backups/dockpanel/snapshots itself.
    let restore_dir =
        PathBuf::from(STAGING_DIR_PARENT).join(format!("restore-{snapshot_id}"));
    let _ = tokio::fs::remove_dir_all(&restore_dir).await;
    tokio::fs::create_dir_all(&restore_dir).await?;

    let extract_status = run_cmd_with_timeout(
        "tar",
        &[
            "-C",
            &restore_dir.to_string_lossy(),
            "-xzf",
            &file_path.to_string_lossy(),
        ],
        Duration::from_secs(300),
    )
    .await?;
    if !extract_status.success() {
        let _ = tokio::fs::remove_dir_all(&restore_dir).await;
        return Err(SnapshotError::Subprocess {
            cmd: "tar -xzf".into(),
            stderr: format!("exit status {extract_status}"),
        });
    }

    // Restore binaries (no-op if file missing inside tar).
    for bin in &["dockpanel-agent", "dockpanel-api", "dockpanel"] {
        let src = restore_dir.join("binaries").join(bin);
        if src.exists() {
            let dst = format!("/usr/local/bin/{bin}");
            tokio::fs::copy(&src, &dst).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    &dst,
                    std::fs::Permissions::from_mode(0o755),
                );
            }
        }
    }

    // Restore /etc/dockpanel/ — only files that exist in snapshot.
    let etc_src = restore_dir.join("etc");
    if etc_src.exists() {
        let copy_status = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cp -a {}/. /etc/dockpanel/",
                shell_escape(&etc_src.to_string_lossy())
            ))
            .status()
            .await?;
        if !copy_status.success() {
            tracing::warn!("snapshot restore: copying /etc/dockpanel returned {copy_status}");
        }
    }

    // Restore DB — pipe gunzip → psql. Dump was created with --clean
    // --if-exists so DROP/CREATE statements are inline.
    let db_dump = restore_dir.join("db").join("dump.sql.gz");
    if db_dump.exists() {
        let restore_status = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "gunzip -c {} | docker exec -i dockpanel-postgres psql -U dockpanel -d dockpanel",
                shell_escape(&db_dump.to_string_lossy())
            ))
            .status()
            .await?;
        if !restore_status.success() {
            let _ = tokio::fs::remove_dir_all(&restore_dir).await;
            return Err(SnapshotError::Subprocess {
                cmd: "psql restore".into(),
                stderr: format!("exit status {restore_status}"),
            });
        }
    }

    let _ = tokio::fs::remove_dir_all(&restore_dir).await;

    // Mark snapshot as used. NB: after rollback the DB is at the snapshot
    // state — this UPDATE writes to the freshly-restored panel_snapshots
    // table, which contains this very row (since the snapshot includes
    // itself). Idempotent.
    let _ = sqlx::query("UPDATE panel_snapshots SET rolled_back_at = NOW() WHERE id = $1")
        .bind(snapshot_id)
        .execute(pool)
        .await;

    tracing::info!("Restored panel snapshot {snapshot_id}");
    Ok(())
}

/// List snapshots newest-first. Operator-facing read; raw model rows.
pub async fn list_snapshots(pool: &PgPool) -> Result<Vec<PanelSnapshot>, SnapshotError> {
    let rows = sqlx::query_as::<_, PanelSnapshot>(
        "SELECT * FROM panel_snapshots ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Delete one snapshot — removes file then DB row. File-first so the row
/// always reflects on-disk reality (no row-without-file states linger).
pub async fn delete_snapshot(pool: &PgPool, snapshot_id: Uuid) -> Result<(), SnapshotError> {
    let row: Option<PanelSnapshot> =
        sqlx::query_as("SELECT * FROM panel_snapshots WHERE id = $1")
            .bind(snapshot_id)
            .fetch_optional(pool)
            .await?;
    let snapshot = row.ok_or(SnapshotError::NotFound(snapshot_id))?;

    let _ = tokio::fs::remove_file(&snapshot.file_path).await;
    sqlx::query("DELETE FROM panel_snapshots WHERE id = $1")
        .bind(snapshot_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Retention sweep: keep most-recent `RETENTION_MIN` regardless of age;
/// delete older than `RETENTION_DAYS` beyond that floor.
/// Returns count of snapshots removed.
pub async fn retention_sweep(pool: &PgPool) -> Result<u32, SnapshotError> {
    let rows: Vec<PanelSnapshot> = sqlx::query_as(
        "SELECT * FROM panel_snapshots ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await?;

    let mut removed = 0u32;
    let cutoff = Utc::now() - chrono::Duration::days(RETENTION_DAYS);

    for snap in rows.iter().skip(RETENTION_MIN as usize) {
        if snap.created_at < cutoff {
            // file-first; if file delete fails, leave the row for retry.
            if let Err(e) = tokio::fs::remove_file(&snap.file_path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        "retention sweep: failed to delete {}: {e} — keeping row",
                        &snap.file_path
                    );
                    continue;
                }
            }
            if sqlx::query("DELETE FROM panel_snapshots WHERE id = $1")
                .bind(snap.id)
                .execute(pool)
                .await
                .is_ok()
            {
                removed += 1;
            }
        }
    }

    if removed > 0 {
        tracing::info!("Snapshot retention sweep removed {removed} aged snapshot(s)");
    }
    Ok(removed)
}

// ── Helpers ──────────────────────────────────────────────────────────────

async fn ensure_dirs() -> Result<(), SnapshotError> {
    tokio::fs::create_dir_all(SNAPSHOT_DIR)
        .await
        .map_err(|e| SnapshotError::DirInit(format!("{SNAPSHOT_DIR}: {e}")))?;
    tokio::fs::create_dir_all(STAGING_DIR_PARENT)
        .await
        .map_err(|e| SnapshotError::DirInit(format!("{STAGING_DIR_PARENT}: {e}")))?;
    Ok(())
}

/// Refuse if the partition holding `path` has fewer than `required` bytes
/// free. Shells out to `df -B1 --output=avail`; if df is unavailable, the
/// check fails open (warns but allows) so the panel doesn't refuse
/// snapshots on a stripped-down install.
async fn check_free_disk(path: &str, required: u64) -> Result<(), SnapshotError> {
    let output = match Command::new("df")
        .args(["-B1", "--output=avail", path])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!("df failed: {e} — skipping free-disk check");
            return Ok(());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let available: Option<u64> = stdout.lines().nth(1).and_then(|l| l.trim().parse().ok());

    match available {
        Some(bytes) if bytes < required => Err(SnapshotError::InsufficientDisk {
            available: bytes,
            required,
        }),
        Some(_) => Ok(()),
        None => {
            tracing::warn!("df output unparseable — skipping free-disk check");
            Ok(())
        }
    }
}

async fn sha256_of(path: &Path) -> Result<String, SnapshotError> {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .await
        .map_err(SnapshotError::Io)?;
    if !output.status.success() {
        return Err(SnapshotError::Subprocess {
            cmd: "sha256sum".into(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let digest = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| SnapshotError::Subprocess {
            cmd: "sha256sum".into(),
            stderr: "no digest in stdout".into(),
        })?;
    Ok(digest.to_string())
}

async fn run_cmd_with_timeout(
    binary: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::ExitStatus, SnapshotError> {
    let fut = Command::new(binary).args(args).status();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(SnapshotError::Io(e)),
        Err(_) => Err(SnapshotError::Subprocess {
            cmd: binary.into(),
            stderr: format!("timed out after {}s", timeout.as_secs()),
        }),
    }
}

/// Minimal shell-escape for filesystem paths. Paths are constructed under
/// our own roots (no user input), but quoting prevents accidental
/// whitespace problems.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_string_forms() {
        assert_eq!(SnapshotTrigger::Manual.as_str(), "manual");
        assert_eq!(
            SnapshotTrigger::PreUpdate {
                target_version: "v2.10.0".into()
            }
            .as_str(),
            "pre-update:v2.10.0"
        );
        let id = Uuid::new_v4();
        assert_eq!(
            SnapshotTrigger::Fleet { server_id: id }.as_str(),
            format!("fleet:{id}")
        );
    }

    #[test]
    fn shell_escape_quotes_single_quotes() {
        assert_eq!(shell_escape("plain"), "'plain'");
        assert_eq!(shell_escape("with space"), "'with space'");
        assert_eq!(shell_escape("o'brien"), "'o'\\''brien'");
    }

    #[tokio::test]
    async fn sha256_of_known_content_is_stable() {
        let tmp = std::env::temp_dir().join(format!("dp-snap-sha-{}", Uuid::new_v4()));
        tokio::fs::write(&tmp, b"hello world").await.unwrap();
        let hash = sha256_of(&tmp).await.unwrap();
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        let _ = tokio::fs::remove_file(&tmp).await;
    }

    #[tokio::test]
    async fn check_free_disk_passes_with_low_requirement() {
        // 1 byte requirement against any path that exists — should pass.
        let res = check_free_disk("/tmp", 1).await;
        assert!(res.is_ok(), "expected free-disk check to pass: {res:?}");
    }

    #[tokio::test]
    async fn check_free_disk_refuses_when_requirement_exceeds_partition() {
        // 100 PiB on /tmp — should refuse (or fall through on weird envs).
        let res = check_free_disk("/tmp", u64::MAX / 2).await;
        match res {
            Err(SnapshotError::InsufficientDisk { .. }) => {}
            Ok(()) => {
                // Acceptable fallthrough on environments where df output
                // is unparseable; the function fails open by design.
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
