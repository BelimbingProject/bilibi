-- Free tags on activities (`core/activity`): a tag is a Base Picklists value's
-- stable code (`core.activity.tags`), and this table is the owner's membership
-- of them. A tenant curates its own tag list, so there is no fixed set of
-- kinds. The pick-lists module cannot see which records hold a code, so a
-- retired tag keeps its memberships here and is marked retired at read time.
CREATE TABLE core_activity_tags (
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 activity_id bigint NOT NULL,
 code text NOT NULL CHECK (octet_length(code) BETWEEN 1 AND 64),
 created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (tenant_id, activity_id, code),
 FOREIGN KEY (tenant_id, activity_id) REFERENCES core_activities(tenant_id, id) ON DELETE CASCADE
);
-- The list's tag filter: which activities hold a code.
CREATE INDEX core_activity_tags_code_idx ON core_activity_tags(tenant_id, code, activity_id);
