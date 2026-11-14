-- A rough city and country resolved once from the operator's local MMDB file
-- when the sign-in is recorded. No file or no match leaves the value null.
ALTER TABLE sessions ADD COLUMN place text;
ALTER TABLE session_sign_ins ADD COLUMN place text;
