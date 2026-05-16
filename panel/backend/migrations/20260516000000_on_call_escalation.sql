-- Phase 4 W3: On-call schedules + escalation policies.
--
-- Adds two new tables and four nullable extensions to existing rows.
-- NULL `escalation_policy_id` on `alert_rules` preserves the prior
-- hardcoded 15-min unack + 30-min re-fire escalation cadence, so every
-- existing install behaves identically until an admin attaches a policy.

CREATE TABLE on_call_schedules (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name          TEXT NOT NULL,
    members       UUID[] NOT NULL,
    cadence_days  INT NOT NULL CHECK (cadence_days BETWEEN 1 AND 90),
    anchor_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE escalation_policies (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name         TEXT NOT NULL,
    steps        JSONB NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE alert_rules
    ADD COLUMN escalation_policy_id UUID NULL
        REFERENCES escalation_policies(id) ON DELETE SET NULL;

-- Three new columns on alerts:
--   acknowledged_by      = actor who acked (NULL = legacy rows; SET NULL on user delete)
--   acknowledged_comment = optional free-text note, 500-char cap enforced at endpoint
--   escalation_step_index = position in the alert_rules.escalation_policy_id chain;
--                           0 means "fresh fire" or "policy NULL"
ALTER TABLE alerts
    ADD COLUMN acknowledged_by UUID NULL
        REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN acknowledged_comment TEXT NULL,
    ADD COLUMN escalation_step_index INT NOT NULL DEFAULT 0;

-- Note: the covering index for "firing AND acknowledged_at IS NULL" already
-- exists as idx_alerts_escalation (20260322400000_alert_escalation.sql). No
-- duplicate index needed here.
