-- A pack compares drafts by tenant and case-insensitive name.
CREATE INDEX base_watchers_name_lookup ON base_watchers (tenant_id, lower(name));
