-- Phase 4 W4: Panel self-update with health-check rollback.
--
-- Adds two new tables (panel_snapshots + fleet_update_runs) and seeds one
-- settings row (`update_channel = 'stable'`). No ALTER on existing structures.
-- Every existing install keeps current behaviour because the new tables are
-- empty and 'stable' is the implicit pre-W4 default.

-- Channel selector default. INSERT-with-ON-CONFLICT-DO-NOTHING so a re-run
-- of the migration never clobbers an operator's choice. Valid values are
-- 'stable' | 'candidate' | 'hold' enforced at the route handler.
INSERT INTO settings (key, value)
    VALUES ('update_channel', 'stable')
    ON CONFLICT (key) DO NOTHING;

-- Persistent snapshots of the panel (binaries + DB dump + /etc/dockpanel/).
-- Survive past the .bak files that update.sh:432-499 deletes on success,
-- so an operator who realizes hours later that the new version broke
-- something has a rollback target.
CREATE TABLE panel_snapshots (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    file_path       TEXT NOT NULL,
    from_version    TEXT NOT NULL,
    -- NULL while update is in flight or if snapshot was taken on demand
    -- without a subsequent update. Filled in when the orchestrator
    -- successfully transitions to Succeeded.
    to_version      TEXT,
    -- 'manual' | 'pre-update' | 'fleet:<server-uuid>'
    trigger         TEXT NOT NULL,
    -- Email of the admin who initiated the snapshot (NULL = system task).
    operator        TEXT,
    size_bytes      BIGINT NOT NULL,
    sha256          TEXT NOT NULL,
    -- Set when this snapshot was used as a rollback target.
    rolled_back_at  TIMESTAMPTZ NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_panel_snapshots_created ON panel_snapshots(created_at DESC);

-- Fleet rolling-update run records. plan = the ordered list of servers to
-- update; progress = per-server status updated as the orchestrator walks
-- the plan. JSONB instead of relational rows because the shape is atomic
-- with the run (no caller ever wants to query "all in-progress steps
-- across all runs" — the run row owns its progress).
CREATE TABLE fleet_update_runs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_version  TEXT NOT NULL,
    plan            JSONB NOT NULL,
    progress        JSONB NOT NULL DEFAULT '[]'::jsonb,
    halt_on_failure BOOLEAN NOT NULL DEFAULT TRUE,
    include_panel   BOOLEAN NOT NULL DEFAULT FALSE,
    started_by      UUID NULL REFERENCES users(id) ON DELETE SET NULL,
    started_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at     TIMESTAMPTZ NULL,
    -- 'success' | 'failed' | 'partial' | NULL while running
    outcome         TEXT NULL
);

CREATE INDEX idx_fleet_update_runs_started ON fleet_update_runs(started_at DESC);
