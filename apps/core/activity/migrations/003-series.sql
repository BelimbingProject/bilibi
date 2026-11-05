-- Repeating series (`core/activity`): the rule a call, meeting or task was
-- created to repeat by. Each occurrence is its own activity, numbered by
-- `series_index`; changing or deleting one leaves the others as they are, so
-- an occurrence moved or removed on its own is the series' exception.
CREATE TABLE core_activity_series (
 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
 tenant_id bigint NOT NULL REFERENCES tenants(id),
 frequency text NOT NULL CHECK (frequency IN ('daily','weekly','monthly')),
 every smallint NOT NULL CHECK (every BETWEEN 1 AND 99),
 -- Weekly: ISO days of the week, 1 Monday to 7 Sunday.
 weekdays smallint[] NOT NULL DEFAULT '{}' CHECK (weekdays <@ ARRAY[1,2,3,4,5,6,7]::smallint[]),
 -- Monthly: the day of the month; a shorter month uses its last day.
 month_day smallint CHECK (month_day BETWEEN 1 AND 31),
 until date,
 occurrences smallint CHECK (occurrences BETWEEN 1 AND 200),
 -- Non-working days of this work calendar (by code) were left out.
 work_calendar text CHECK (work_calendar IS NULL OR octet_length(work_calendar) BETWEEN 1 AND 100),
 -- The zone the wall-clock time repeats in.
 timezone text NOT NULL CHECK (octet_length(timezone) BETWEEN 1 AND 64),
 created_by bigint NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 UNIQUE (tenant_id, id),
 FOREIGN KEY (tenant_id, created_by) REFERENCES users(tenant_id, id),
 -- It ends on a date or after a number of occurrences, never open-ended.
 CHECK (num_nonnulls(until, occurrences) = 1),
 CHECK ((frequency = 'monthly') = (month_day IS NOT NULL)),
 CHECK (frequency = 'weekly' OR weekdays = '{}')
);

ALTER TABLE core_activities
 ADD COLUMN series_id bigint,
 ADD COLUMN series_index smallint CHECK (series_index BETWEEN 1 AND 200),
 ADD FOREIGN KEY (tenant_id, series_id) REFERENCES core_activity_series(tenant_id, id),
 ADD CHECK ((series_id IS NULL) = (series_index IS NULL));
CREATE UNIQUE INDEX core_activities_series_idx ON core_activities(tenant_id, series_id, series_index) WHERE series_id IS NOT NULL;
