-- People named directly on a scheduled report, beside the holders of its roles
-- (CC people). Each still receives the report under their own access, and
-- nothing once they lose it.
CREATE TABLE base_report_subscription_people (
 tenant_id bigint NOT NULL,
 subscription_id bigint NOT NULL,
 user_id bigint NOT NULL CHECK (user_id > 0),
 CONSTRAINT base_report_subscription_people_pkey PRIMARY KEY (tenant_id, subscription_id, user_id),
 CONSTRAINT base_report_subscription_people_subscription_fkey FOREIGN KEY (tenant_id, subscription_id) REFERENCES base_report_subscriptions (tenant_id, id) ON DELETE CASCADE
);
