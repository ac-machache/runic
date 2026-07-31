CREATE SCHEMA IF NOT EXISTS runic;

ALTER TABLE sessions       SET SCHEMA runic;
ALTER TABLE session_events SET SCHEMA runic;
ALTER TABLE chat_messages  SET SCHEMA runic;
ALTER TABLE artifacts      SET SCHEMA runic;
ALTER TABLE runs           SET SCHEMA runic;

ALTER TABLE runic.session_events RENAME TO events;
ALTER TABLE runic.chat_messages  RENAME TO chats;

ALTER INDEX runic.chat_messages_tsv_idx    RENAME TO chats_tsv_idx;
ALTER INDEX runic.chat_messages_recent_idx RENAME TO chats_recent_idx;
