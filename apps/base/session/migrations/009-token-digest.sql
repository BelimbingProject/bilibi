-- A session row keeps only the SHA-256 of its cookie token, in lowercase hex
-- (`bilibi_session::session_id`), never the token: whoever reads this table
-- (the SQL console, a backup, a replica) learns no bearer. Rows stored
-- before this held the token itself, so they end here and their holders sign
-- in again. The check refuses a raw token written by mistake.
DELETE FROM sessions;
ALTER TABLE sessions ADD CONSTRAINT sessions_id_digest CHECK (id ~ '^[0-9a-f]{64}$');
