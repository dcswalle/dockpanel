-- Agent auto-update: opt-in, and OFF unless an operator switches it on.
--
-- The panel answers GET /api/agent/version with a null target unless this is
-- 'true', so this row is the switch that reaches every remote agent — there is
-- no panel->agent configuration push, an agent only learns things by asking.
INSERT INTO settings (key, value) VALUES ('agent_auto_update_enabled', 'false')
ON CONFLICT (key) DO NOTHING;

-- Retire the three keys the old endpoint read. Nothing ever wrote them except a
-- hand-crafted admin settings PUT, and the reader is gone: the updater now
-- derives the asset URL and its expected digest on the box from the release's
-- own checksums.txt, which is also the only form that can be correct for a
-- mixed amd64/arm64 fleet. Left behind they would be misleading dead rows.
DELETE FROM settings
WHERE key IN ('agent_latest_version', 'agent_download_url', 'agent_checksum');
