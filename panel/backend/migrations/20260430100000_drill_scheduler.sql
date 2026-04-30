-- Phase 4 W1.2.d: Per-policy drill scheduler
--
-- Drills run on a separate cadence from backups. Default: 04:00 Sunday UTC
-- (weekly low-traffic window). Scheduler dispatches db + volume drills only
-- (site backups don't carry policy_id; passive verifier covers them every 6h).

ALTER TABLE backup_policies
    ADD COLUMN IF NOT EXISTS drill_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN IF NOT EXISTS drill_schedule TEXT NOT NULL DEFAULT '0 4 * * 0',
    ADD COLUMN IF NOT EXISTS last_drill_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_backup_policies_drill_enabled
    ON backup_policies(drill_enabled) WHERE drill_enabled = TRUE;
