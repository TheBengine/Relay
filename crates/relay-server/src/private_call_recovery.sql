-- Accepted signalling retains its original ID. Ringing invitations remain volatile.
CREATE TABLE private_call_recovery(
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    caller_id TEXT NOT NULL REFERENCES users(id),
    recipient_id TEXT NOT NULL REFERENCES users(id),
    state TEXT NOT NULL CHECK(state IN('accepted','ended')),
    join_expires INTEGER NOT NULL,
    retained_until INTEGER NOT NULL,
    CHECK(caller_id!=recipient_id)
);
ALTER TABLE voice_resume_leases ADD COLUMN private_call_id TEXT REFERENCES private_call_recovery(id) ON DELETE CASCADE;
CREATE INDEX private_call_lease ON voice_resume_leases(private_call_id);
CREATE TRIGGER private_call_budget BEFORE INSERT ON private_call_recovery
WHEN (SELECT COUNT(*) FROM private_call_recovery)>=1024
BEGIN SELECT RAISE(ABORT,'Private call recovery capacity reached'); END;
CREATE TRIGGER private_call_ended AFTER UPDATE OF state ON private_call_recovery
WHEN new.state='ended'
BEGIN DELETE FROM voice_resume_leases WHERE private_call_id=new.id; END;
CREATE TRIGGER private_call_no_resurrection BEFORE UPDATE OF state ON private_call_recovery
WHEN old.state='ended' AND new.state!='ended'
BEGIN SELECT RAISE(ABORT,'Private call already ended'); END;
CREATE TRIGGER private_call_lease_insert BEFORE INSERT ON voice_resume_leases
WHEN new.private_call_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM private_call_recovery c WHERE c.id=new.private_call_id AND c.state='accepted'
    AND c.channel_id=new.channel_id AND new.user_id IN(c.caller_id,c.recipient_id))
BEGIN SELECT RAISE(ABORT,'Invalid private call binding'); END;
CREATE TRIGGER private_call_lease_update BEFORE UPDATE OF private_call_id,channel_id,user_id ON voice_resume_leases
WHEN new.private_call_id IS NOT NULL AND NOT EXISTS(
    SELECT 1 FROM private_call_recovery c WHERE c.id=new.private_call_id AND c.state='accepted'
    AND c.channel_id=new.channel_id AND new.user_id IN(c.caller_id,c.recipient_id))
BEGIN SELECT RAISE(ABORT,'Invalid private call binding'); END;

-- Explicit revocation is irreversible even if the permission/relationship is
-- restored before a reconnect. End first; its trigger removes BOTH exact leases.
CREATE TRIGGER private_call_session_revoke BEFORE DELETE ON sessions
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30
    WHERE state='accepted' AND id IN(SELECT private_call_id FROM voice_resume_leases WHERE token_hash=old.token_hash); END;
CREATE TRIGGER private_call_device_revoke BEFORE UPDATE OF active ON crypto_devices WHEN new.active=0
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30
    WHERE state='accepted' AND id IN(SELECT private_call_id FROM voice_resume_leases WHERE device_id=new.device_id); END;
CREATE TRIGGER private_call_permission_insert BEFORE INSERT ON channel_permissions
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30 WHERE state='accepted' AND channel_id=new.channel_id; END;
CREATE TRIGGER private_call_permission_update BEFORE UPDATE ON channel_permissions
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30 WHERE state='accepted' AND channel_id IN(old.channel_id,new.channel_id); END;
CREATE TRIGGER private_call_permission_delete BEFORE DELETE ON channel_permissions
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30 WHERE state='accepted' AND channel_id=old.channel_id; END;
CREATE TRIGGER private_call_unfriend BEFORE DELETE ON friendships
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30
    WHERE state='accepted' AND ((caller_id=old.sender AND recipient_id=old.recipient) OR (caller_id=old.recipient AND recipient_id=old.sender)); END;
CREATE TRIGGER private_call_block BEFORE INSERT ON user_blocks
BEGIN UPDATE private_call_recovery SET state='ended',retained_until=unixepoch()+30
    WHERE state='accepted' AND ((caller_id=new.owner AND recipient_id=new.peer) OR (caller_id=new.peer AND recipient_id=new.owner)); END;

-- A schema-22 private lease has no call ID. It cannot safely be assigned to a
-- pair's next call. Interrupt those legacy private admissions once at upgrade.
DELETE FROM voice_resume_leases WHERE channel_id IN(SELECT channel_id FROM conversations WHERE kind='group_voice');
