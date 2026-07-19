#!/usr/bin/env bash
#
# DockPanel — restore a panel snapshot (binaries + /etc/dockpanel + database).
#
# This script is the ONLY thing that mutates state during a rollback. It is
# compiled into dockpanel-api (`include_str!`) and written to
# /var/lib/dockpanel/restore-snapshot.sh at rollback time, so it cannot drift
# from the binary that invokes it and does not depend on /opt/dockpanel being
# present (see lesson #35/#38 — a repo-side config that no installer deploys is
# dev fiction; embedding it removes the deployment step entirely).
#
# WHY A SEPARATE, DETACHED PROCESS (the whole point):
#   The restore used to run INLINE inside the dockpanel-api HTTP handler. A
#   restore takes longer than the panel's own 300s request timeout
#   (TimeoutLayer, backend main.rs; nginx proxy_read_timeout 300s) — measured at
#   394.9s on a lab box. At 300s axum dropped the request future, which broke the
#   `gunzip | docker exec ... psql` pipe; psql then read a clean EOF and EXITED 0,
#   so the code recorded a SUCCESSFUL restore while the database had been left
#   with 1 of 92 tables. Every pre-update snapshot the panel ever took was
#   unrestorable, and the failure was indistinguishable from success.
#
#   So: PID1 owns this process (systemd-run --collect, per lesson #47 — a
#   transient *service*, never a session-owned scope, which would die with the
#   caller), it outlives the api it stops, and the database stage is atomic and
#   status-checked (lesson #45).
#
# WHAT A ROLLBACK DOES AND DOES NOT DO (verified on a lab box, not assumed):
#   It restores what the snapshot CONTAINS — the three binaries, /etc/dockpanel,
#   and the database. The database is a true point-in-time revert: the `public`
#   schema is dropped and rebuilt from the dump inside one transaction, so an
#   object created after the snapshot does NOT survive.
#
#   That is a deliberate change from v2.11.5/v2.11.6, where the restore applied
#   only the dump's own DROP statements and therefore MERGED the snapshot into
#   whatever was there. Post-snapshot tables outlived the rollback while
#   _sqlx_migrations was rewound, and a later forward update then re-ran a
#   migration whose objects already existed: the api panicked at startup and
#   crash-looped into a permanent 502. Reproduced end to end before this was
#   changed. The data those extra tables held is not silently discarded — the
#   pre-rollback dump written below captures the full pre-restore database.
#
#   Nothing outside the snapshot is touched at all: /etc/nginx, /etc/letsencrypt,
#   /var/www, docker volumes and site data are NOT rewound. A rollback is "put the
#   panel back", not "put the machine back".
#
# Env (all set by the caller):
#   DOCKPANEL_SNAPSHOT_ID        uuid of the snapshot row
#   DOCKPANEL_SNAPSHOT_TARBALL   absolute path to the .tar.gz
#   DOCKPANEL_SNAPSHOT_SHA256    expected sha256 of that tarball
# Optional overrides: DOCKPANEL_PG_CONTAINER / _PG_USER / _PG_DB
#
# Result is always written to /var/lib/dockpanel/last-restore.json.
# Exit 0 = restored; non-zero = nothing lost (see "stage" in the result file).

set -euo pipefail

SNAP_ID="${DOCKPANEL_SNAPSHOT_ID:?DOCKPANEL_SNAPSHOT_ID is required}"
TARBALL="${DOCKPANEL_SNAPSHOT_TARBALL:?DOCKPANEL_SNAPSHOT_TARBALL is required}"
EXPECT_SHA="${DOCKPANEL_SNAPSHOT_SHA256:?DOCKPANEL_SNAPSHOT_SHA256 is required}"
PG_CONTAINER="${DOCKPANEL_PG_CONTAINER:-dockpanel-postgres}"
# Deliberately mirrors the pg_dump invocation that CREATED the snapshot
# (panel_snapshot.rs build_snapshot_inner) — restore must be symmetric with dump.
PG_USER="${DOCKPANEL_PG_USER:-dockpanel}"
PG_DB="${DOCKPANEL_PG_DB:-dockpanel}"

STATE_DIR=/var/lib/dockpanel
RESULT="$STATE_DIR/last-restore.json"
WORK="$STATE_DIR/restore-$SNAP_ID"
LOG_TAG=dockpanel-restore
BINS=(dockpanel-agent dockpanel-api dockpanel)

mkdir -p "$STATE_DIR"
chmod 0700 "$STATE_DIR" 2>/dev/null || true

stage="init"
finished=0
services_stopped=0

log() { echo "[restore] $*"; command -v systemd-cat >/dev/null 2>&1 && echo "[restore] $*" | systemd-cat -t "$LOG_TAG" || true; }

# JSON-escape a string for the single "detail" field (quotes, backslashes,
# control chars). Kept to one line so the result file stays trivially parseable.
json_escape() {
    printf '%s' "$1" | tr -d '\000-\010\013\014\016-\037' \
        | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | tr '\n' ' '
}

write_result() {
    local ok="$1" st="$2" detail="$3"
    local tmp="$RESULT.tmp"
    printf '{"snapshot_id":"%s","ok":%s,"stage":"%s","detail":"%s","finished_at":"%s"}\n' \
        "$SNAP_ID" "$ok" "$st" "$(json_escape "$detail")" \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$tmp"
    chmod 0600 "$tmp" 2>/dev/null || true
    mv "$tmp" "$RESULT"
}

fail() {
    local detail="$1"
    log "FAILED at stage '$stage': $detail"
    write_result false "$stage" "$detail"
    finished=1
    exit 1
}

# Safety net (mirrors scripts/update.sh): if we die anywhere between stopping and
# starting the services, bring them back rather than leaving the box dark — the
# s228 BUG I failure shape (a test box sat down for 47 minutes).
on_exit() {
    local code=$?
    if [ "$services_stopped" = "1" ]; then
        log "restarting services from exit trap"
        systemctl start dockpanel-agent 2>/dev/null || true
        systemctl start dockpanel-api 2>/dev/null || true
    fi
    if [ "$finished" != "1" ]; then
        write_result false "$stage" "aborted at stage '$stage' with exit code $code"
    fi
    rm -rf "$WORK" 2>/dev/null || true
}
trap on_exit EXIT INT TERM

# Belt-and-braces: if invoked by hand from inside the panel's own cgroup, escape
# it first. The normal path already arrives here under systemd-run.
if [ -z "${DOCKPANEL_RESTORE_DETACHED:-}" ] && command -v systemd-run >/dev/null 2>&1; then
    if grep -qE 'dockpanel-(api|agent)\.service' /proc/self/cgroup 2>/dev/null; then
        log "re-executing outside the panel's service cgroup"
        trap - EXIT INT TERM
        exec systemd-run --quiet --collect \
            --unit="dockpanel-snapshot-restore-$SNAP_ID" \
            --setenv=DOCKPANEL_RESTORE_DETACHED=1 \
            --setenv=DOCKPANEL_SNAPSHOT_ID="$SNAP_ID" \
            --setenv=DOCKPANEL_SNAPSHOT_TARBALL="$TARBALL" \
            --setenv=DOCKPANEL_SNAPSHOT_SHA256="$EXPECT_SHA" \
            --setenv=DOCKPANEL_PG_CONTAINER="$PG_CONTAINER" \
            --setenv=DOCKPANEL_PG_USER="$PG_USER" \
            --setenv=DOCKPANEL_PG_DB="$PG_DB" \
            bash "$0"
    fi
fi

write_result false "started" "restore in progress"
log "restoring snapshot $SNAP_ID from $TARBALL"

# ── 1. Verify the tarball ────────────────────────────────────────────────────
stage="verify-tarball"
[ -f "$TARBALL" ] || fail "tarball not found at $TARBALL"
ACTUAL_SHA="$(sha256sum "$TARBALL" | awk '{print $1}')"
[ "$ACTUAL_SHA" = "$EXPECT_SHA" ] || fail "sha256 mismatch: expected $EXPECT_SHA, got $ACTUAL_SHA"
log "sha256 verified"

# ── 2. Extract ───────────────────────────────────────────────────────────────
stage="extract"
rm -rf "$WORK"
mkdir -p "$WORK"
chmod 0700 "$WORK"
tar -C "$WORK" -xzf "$TARBALL" || fail "tar extraction failed"

DUMP_GZ="$WORK/db/dump.sql.gz"
DUMP_SQL="$WORK/db/dump.sql"

# ── 3. Prove the dump is complete BEFORE destroying anything ─────────────────
# Decompressing to a file first also removes the pipe that made truncation
# invisible: psql is fed from a regular file, so a broken producer cannot present
# a short read as a clean end-of-input.
stage="verify-dump"
if [ -f "$DUMP_GZ" ]; then
    gunzip -c "$DUMP_GZ" > "$DUMP_SQL" || fail "gunzip of the database dump failed"
    # The window is deliberately wider than the marker's current position. pg_dump
    # writes the marker near the end but not necessarily last: PostgreSQL's
    # August-2025 minors added a trailing `\unrestrict` line, which on the lab put
    # the marker at line 6243 of 6247 — inside a 5-line window by exactly zero
    # lines to spare. One more trailer from any future pg_dump and this check
    # would reject EVERY snapshot as truncated, breaking rollback completely.
    # It stays a tail window rather than a whole-file grep on purpose: this panel
    # stores operator-authored text, so the marker string can legitimately appear
    # inside the data and a whole-file grep could not tell that from a real end.
    if ! tail -20 "$DUMP_SQL" | grep -q 'PostgreSQL database dump complete'; then
        fail "database dump is truncated (completion marker absent) — refusing to apply it"
    fi
    EXPECT_TABLES="$(grep -c '^CREATE TABLE' "$DUMP_SQL" || true)"
    log "dump verified complete: $EXPECT_TABLES CREATE TABLE statements"
else
    EXPECT_TABLES=0
    log "snapshot contains no database dump — skipping the database stage"
fi

# ── 4. Stop services ─────────────────────────────────────────────────────────
# Everything below runs with nothing executing from /usr/local/bin, which is what
# makes the binary swap safe (lesson #48: you cannot write over a running
# executable; rename works, but not running at all is better still).
stage="stop-services"
log "stopping dockpanel-api and dockpanel-agent"
services_stopped=1
systemctl stop dockpanel-api 2>/dev/null || true
systemctl stop dockpanel-agent 2>/dev/null || true

# ── 5. Database (the destructive stage; atomic) ──────────────────────────────
# ON_ERROR_STOP=1 + --single-transaction is the difference between a restore that
# can destroy the database while reporting success and one that either fully
# applies or changes nothing. Verified both ways on a lab box against a
# deliberately truncated stream: without the flags psql exited 0 having left 1 of
# 92 tables; with them it exited 3 and all 92 survived.
stage="database"
if [ -f "$DUMP_SQL" ]; then
    docker exec "$PG_CONTAINER" pg_isready -U "$PG_USER" -d "$PG_DB" >/dev/null 2>&1 \
        || fail "postgres container '$PG_CONTAINER' is not accepting connections"

    # Undo for the undo. The transaction below makes a FAILED restore harmless,
    # but a SUCCESSFUL one is still a destructive act the operator may regret —
    # so capture where they were first. pipefail because the exit status of
    # `pg_dump | gzip` is gzip's, and gzip exits 0 on a truncated stream.
    stage="pre-rollback-dump"
    PRE_DUMP="$STATE_DIR/pre-rollback-$SNAP_ID.sql.gz"
    if bash -c "set -o pipefail; docker exec '$PG_CONTAINER' pg_dump -U '$PG_USER' --clean --if-exists '$PG_DB' | gzip > '$PRE_DUMP'"; then
        chmod 0600 "$PRE_DUMP" 2>/dev/null || true
        log "pre-rollback state saved to $PRE_DUMP"
        # These are now the only way back from a rollback, so they are worth
        # keeping — but nothing else in the product ever deletes them (the
        # retention sweep walks panel_snapshots rows and tarballs, not this
        # directory), and one is written on every rollback. Keep the newest few.
        # A failed prune must never fail a restore, so it is reported, not fatal.
        ls -1t "$STATE_DIR"/pre-rollback-*.sql.gz 2>/dev/null | tail -n +4 | while read -r old; do
            rm -f "$old" || log "could not prune stale pre-rollback dump $old"
        done
    else
        fail "could not capture the pre-rollback database state — refusing to proceed"
    fi

    stage="database"
    log "restoring database (atomic, $EXPECT_TABLES tables expected)"
    # The schema is torn down first, in the SAME transaction as the dump.
    #
    # `pg_dump --clean` only emits DROP statements for objects the dump itself
    # contains, so before this, anything created AFTER the snapshot outlived a
    # restore of it. That left a database that matched neither version: the newer
    # version's tables were still present while `_sqlx_migrations` had been
    # rewound to the older state, so a later forward update re-ran a migration
    # whose objects already existed. MEASURED on a lab box: the api panicked at
    # startup with `relation "..." already exists`, exited 101, and crash-looped
    # under Restart=always until it hit StartLimitBurst — a permanent 502 from a
    # rollback that had reported success. Dropping the schema makes a restore an
    # actual point-in-time revert instead of a merge.
    #
    # Owner and ACL are restored explicitly because the dump carries neither: a
    # bare CREATE SCHEMA would hand `public` to the restoring role and drop
    # PUBLIC's USAGE grant, silently changing the database's permissions on every
    # rollback. Verified to reproduce the original owner and ACL exactly.
    #
    # This is still all-or-nothing. The DROP is inside psql's --single-transaction,
    # so a dump that fails to apply rolls the teardown back with it and the
    # database is left exactly as it was. The pre-rollback dump taken above is the
    # second line of defence.
    # Assembled into a REGULAR FILE and fed to psql by redirection — never piped.
    # A pipe would put a live producer back in front of psql, which is the whole
    # defect this file exists to prevent: if the producer dies mid-stream psql
    # sees a clean EOF, and --single-transaction then COMMITS a dropped schema
    # plus half a dump. `set +e` below suppresses errexit but NOT pipefail, so the
    # status would also be the producer's rather than psql's, and the "nothing
    # changed" message below would be printed over a destroyed database. Same
    # reason the dump itself is decompressed to a file at the verify-dump stage.
    PREAMBLE_SQL="$WORK/db/preamble.sql"
    APPLY_SQL="$WORK/db/apply.sql"
    printf '%s\n' \
        'DROP SCHEMA IF EXISTS public CASCADE;' \
        'CREATE SCHEMA public;' \
        'ALTER SCHEMA public OWNER TO pg_database_owner;' \
        'GRANT USAGE ON SCHEMA public TO PUBLIC;' \
        > "$PREAMBLE_SQL" || fail "could not stage the schema teardown — nothing changed"
    cat "$PREAMBLE_SQL" "$DUMP_SQL" > "$APPLY_SQL" \
        || fail "could not assemble the restore stream — nothing changed"
    # Byte-exact, so a short write is caught before anything is destroyed rather
    # than presented to psql as a complete input.
    A_SZ="$(stat -c%s "$APPLY_SQL")"; P_SZ="$(stat -c%s "$PREAMBLE_SQL")"; D_SZ="$(stat -c%s "$DUMP_SQL")"
    if [ "$A_SZ" != "$(( P_SZ + D_SZ ))" ]; then
        fail "assembled restore stream is short ($A_SZ vs $(( P_SZ + D_SZ )) bytes) — nothing changed"
    fi
    set +e
    docker exec -i "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" \
        -X -q -v ON_ERROR_STOP=1 --single-transaction \
        < "$APPLY_SQL" > "$WORK/psql.out" 2> "$WORK/psql.err"
    DB_RC=$?
    set -e
    if [ "$DB_RC" != "0" ]; then
        # The transaction rolled back: the database is exactly as it was, and
        # no binary or config has been touched yet. Nothing is lost.
        fail "database restore failed (psql exit $DB_RC), transaction rolled back, nothing changed: $(tail -3 "$WORK/psql.err" | tr '\n' ' ')"
    fi

    # "psql exited 0" is not the success condition — the schema is (lesson #45).
    #
    # Deliberately still a floor, even though the teardown now makes a surplus
    # structurally impossible. This check runs AFTER the transaction has
    # committed and BEFORE the binaries are reverted, so a `fail` here trips the
    # exit trap, which restarts the still-installed NEWER api against the
    # just-reverted database — and that api migrates it forward again, quietly
    # undoing the rollback it is reporting as failed. A check that cannot be
    # wrong in that window is worth more than a tighter one that can: an
    # equality would trip on a table the dump creates outside `public`, or on
    # any counting mismatch between the grep and the catalogue query.
    stage="database-verify"
    GOT_TABLES="$(docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAq \
        -c "select count(*) from information_schema.tables where table_schema='public' and table_type='BASE TABLE'" 2>/dev/null | tr -d ' \r')"
    if [ -z "$GOT_TABLES" ] || [ "$GOT_TABLES" -lt "$EXPECT_TABLES" ]; then
        fail "post-restore schema check failed: expected >= $EXPECT_TABLES tables, found ${GOT_TABLES:-none}"
    fi
    log "database restored and verified: $GOT_TABLES tables"

    # Put the snapshot we just restored FROM back into the restored database,
    # and mark it used.
    #
    # Two things conspire here. A snapshot's dump is taken BEFORE its own row is
    # inserted, so the dump does not contain that row — restoring therefore
    # DELETES the very snapshot the operator restored from, leaving its tarball
    # on disk with nothing listing it. And a stamp written before the restore is
    # overwritten by the restore itself. Both were observed on a lab box
    # (rolled_back_at came back empty and the row was gone). So the row is
    # re-established here, from the tarball we can see and the metadata inside
    # it, and marked as rolled back.
    stage="record-rollback"
    sqlq() { printf "%s" "$1" | sed "s/'/''/g"; }
    meta_field() {
        sed -n "s/.*\"$1\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" \
            "$WORK/metadata.json" 2>/dev/null | head -1
    }
    M_FROM="$(meta_field from_version)"; [ -n "$M_FROM" ] || M_FROM="unknown"
    M_TRIG="$(meta_field trigger)";      [ -n "$M_TRIG" ] || M_TRIG="manual"
    M_CREATED="$(meta_field created_at)"
    M_OPER="$(meta_field operator)"
    SIZE_BYTES="$(stat -c%s "$TARBALL" 2>/dev/null || echo 0)"
    if [ -n "$M_OPER" ]; then OPER_SQL="'$(sqlq "$M_OPER")'"; else OPER_SQL="NULL"; fi
    if [ -n "$M_CREATED" ]; then CREATED_SQL="'$(sqlq "$M_CREATED")'"; else CREATED_SQL="NOW()"; fi

    docker exec "$PG_CONTAINER" psql -U "$PG_USER" -d "$PG_DB" -tAq -v ON_ERROR_STOP=1 -c \
"INSERT INTO panel_snapshots (id, file_path, from_version, trigger, operator, size_bytes, sha256, created_at, rolled_back_at)
 VALUES ('$SNAP_ID', '$(sqlq "$TARBALL")', '$(sqlq "$M_FROM")', '$(sqlq "$M_TRIG")', $OPER_SQL, $SIZE_BYTES, '$(sqlq "$EXPECT_SHA")', $CREATED_SQL, NOW())
 ON CONFLICT (id) DO UPDATE SET rolled_back_at = NOW()" >/dev/null \
        || fail "restore applied but the rollback could not be recorded in panel_snapshots"
    log "recorded rollback against snapshot $SNAP_ID"
fi

# ── 6. Binaries ──────────────────────────────────────────────────────────────
# Services are stopped, so a plain mv is safe and atomic. Each binary keeps a
# .prerestore copy. No `|| true` anywhere: a restore that cannot restore must say
# so (lesson #48).
stage="binaries"
for bin in "${BINS[@]}"; do
    src="$WORK/binaries/$bin"
    dst="/usr/local/bin/$bin"
    if [ ! -f "$src" ]; then
        log "snapshot has no $bin — leaving the installed one in place"
        continue
    fi
    if [ -f "$dst" ]; then
        cp -a "$dst" "$dst.prerestore" || fail "could not back up $dst"
    fi
    install -m 0755 "$src" "$dst.restoring" || fail "could not stage $bin"
    mv "$dst.restoring" "$dst" || fail "could not move $bin into place"
    log "restored $dst"
done

# ── 7. /etc/dockpanel ────────────────────────────────────────────────────────
# Treated as fatal, not best-effort: api.env carries JWT_SECRET and the database
# password, so a half-applied /etc against a fully-restored database is a broken
# panel, not a warning.
stage="etc"
if [ -d "$WORK/etc" ]; then
    cp -a /etc/dockpanel "/etc/dockpanel.prerestore.$SNAP_ID" 2>/dev/null || true
    cp -a "$WORK/etc/." /etc/dockpanel/ || fail "restoring /etc/dockpanel failed"
    log "restored /etc/dockpanel"
fi

# ── 8. Start + prove it came back ────────────────────────────────────────────
stage="start-services"
systemctl daemon-reload 2>/dev/null || true
systemctl start dockpanel-agent || fail "dockpanel-agent failed to start after restore"
sleep 1
systemctl start dockpanel-api || fail "dockpanel-api failed to start after restore"
services_stopped=0

stage="health"
HEALTH=""
for _ in $(seq 1 30); do
    # `if` rather than `|| true`: a per-attempt failure is expected while the
    # panel boots, but it must not be spelled the same way as a swallowed error.
    if HEALTH="$(curl -fsS -m 3 http://127.0.0.1:3080/api/health 2>/dev/null)"; then
        [ -n "$HEALTH" ] && break
    fi
    sleep 2
done
[ -n "$HEALTH" ] || fail "panel did not answer /api/health within 60s after restore"

log "restore complete: $HEALTH"
write_result true "complete" "restored snapshot $SNAP_ID; health: $HEALTH"
finished=1
exit 0
