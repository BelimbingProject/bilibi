-- Tasks, calls, meetings and notes, and the people taking part in them:
-- `core/activity`. Each activity may point at one record in any area through
-- a `base/records` reference (`related_kind`, `related_id`); no foreign key
-- crosses to that record's module. Only the creator, the assignee and account
-- guests see an activity, and readers of the related record when `shared`.
CREATE TABLE core_activities (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 company_id bigint NOT NULL,
 kind text NOT NULL CHECK (kind IN ('call','meeting','task','note')),
 subject text NOT NULL CHECK (length(subject) BETWEEN 1 AND 160),
 status text,
 priority text CHECK (priority IN ('high','medium','low')),
 direction text CHECK (direction IN ('inbound','outbound')),
 starts_at timestamptz,
 ends_at timestamptz,
 due_at timestamptz,
 start_at timestamptz,
 location text CHECK (location IS NULL OR length(location) BETWEEN 1 AND 255),
 join_url text CHECK (join_url IS NULL OR length(join_url) BETWEEN 1 AND 2000),
 meeting_type text CHECK (meeting_type IS NULL OR length(meeting_type) BETWEEN 1 AND 100),
 call_outcome text CHECK (call_outcome IS NULL OR length(call_outcome) BETWEEN 1 AND 100),
 description text CHECK (description IS NULL OR length(description) BETWEEN 1 AND 10000),
 internal_notes text CHECK (internal_notes IS NULL OR length(internal_notes) BETWEEN 1 AND 10000),
 related_kind text CHECK (related_kind IS NULL OR octet_length(related_kind) BETWEEN 1 AND 100),
 related_id text CHECK (related_id IS NULL OR octet_length(related_id) BETWEEN 1 AND 64),
 shared boolean NOT NULL DEFAULT false,
 assignee_user_id bigint NOT NULL,
 created_by bigint NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(),
 closed_at timestamptz,
 reminded_at timestamptz,
 deleted_at timestamptz,
 version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id),
 FOREIGN KEY (tenant_id, assignee_user_id) REFERENCES users(tenant_id, id),
 FOREIGN KEY (tenant_id, created_by) REFERENCES users(tenant_id, id),
 -- Each kind has its own statuses; a note has none.
 CHECK (
  (kind IN ('call','meeting') AND status IN ('scheduled','held','not_held','canceled'))
  OR (kind = 'task' AND status IN ('not_started','in_progress','pending_input','completed','deferred'))
  OR (kind = 'note' AND status IS NULL)
 ),
 CHECK ((kind = 'task') = (priority IS NOT NULL)),
 CHECK ((kind = 'call') = (direction IS NOT NULL)),
 CHECK (kind NOT IN ('call','meeting') OR (starts_at IS NOT NULL AND ends_at IS NOT NULL AND starts_at < ends_at)),
 CHECK (kind IN ('call','meeting') OR (starts_at IS NULL AND ends_at IS NULL)),
 CHECK (kind = 'task' OR (due_at IS NULL AND start_at IS NULL)),
 CHECK (kind = 'meeting' OR (location IS NULL AND join_url IS NULL AND meeting_type IS NULL)),
 CHECK (kind = 'call' OR call_outcome IS NULL),
 CHECK ((related_kind IS NULL) = (related_id IS NULL)),
 -- Shared with the readers of a related record, so there must be one.
 CHECK (NOT shared OR related_kind IS NOT NULL),
 -- Closed exactly when the status is a closing one.
 CHECK ((closed_at IS NOT NULL) = COALESCE(status IN ('held','not_held','canceled','completed','deferred'), false))
);
CREATE INDEX core_activities_related_idx ON core_activities(tenant_id, related_kind, related_id);
CREATE INDEX core_activities_assignee_idx ON core_activities(tenant_id, assignee_user_id) WHERE deleted_at IS NULL;
-- The reminder sweep: open activities not yet reminded, by when they are due.
CREATE INDEX core_activities_remind_idx ON core_activities(tenant_id, (COALESCE(starts_at, due_at))) WHERE deleted_at IS NULL AND closed_at IS NULL AND reminded_at IS NULL AND kind <> 'note';
-- The bell's To do: the assignee's reminded open activities.
CREATE INDEX core_activities_reminded_idx ON core_activities(tenant_id, assignee_user_id) WHERE deleted_at IS NULL AND closed_at IS NULL AND reminded_at IS NOT NULL;
CREATE INDEX core_activities_creator_idx ON core_activities(tenant_id, created_by) WHERE deleted_at IS NULL;

-- Who takes part: an account, a Core person, or an email address (a lead
-- with no Core person yet). One row per guest and activity.
CREATE TABLE core_activity_guests (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 activity_id bigint NOT NULL,
 guest_user_id bigint,
 guest_person_id bigint,
 guest_email text CHECK (guest_email IS NULL OR octet_length(guest_email) BETWEEN 3 AND 254),
 role text NOT NULL CHECK (role IN ('creator','assignee','related','invited')),
 response text NOT NULL DEFAULT 'none' CHECK (response IN ('none','accepted','declined','tentative')),
 invited_at timestamptz,
 created_at timestamptz NOT NULL DEFAULT now(),
 CHECK (num_nonnulls(guest_user_id, guest_person_id, guest_email) = 1),
 FOREIGN KEY (tenant_id, activity_id) REFERENCES core_activities(tenant_id, id),
 FOREIGN KEY (tenant_id, guest_user_id) REFERENCES users(tenant_id, id),
 FOREIGN KEY (tenant_id, guest_person_id) REFERENCES persons(tenant_id, id)
);
CREATE UNIQUE INDEX core_activity_guests_user_idx ON core_activity_guests(tenant_id, activity_id, guest_user_id) WHERE guest_user_id IS NOT NULL;
CREATE UNIQUE INDEX core_activity_guests_person_idx ON core_activity_guests(tenant_id, activity_id, guest_person_id) WHERE guest_person_id IS NOT NULL;
CREATE UNIQUE INDEX core_activity_guests_email_idx ON core_activity_guests(tenant_id, activity_id, lower(guest_email)) WHERE guest_email IS NOT NULL;
CREATE INDEX core_activity_guests_user_lookup_idx ON core_activity_guests(tenant_id, guest_user_id) WHERE guest_user_id IS NOT NULL;
CREATE INDEX core_activity_guests_person_lookup_idx ON core_activity_guests(tenant_id, guest_person_id) WHERE guest_person_id IS NOT NULL;
