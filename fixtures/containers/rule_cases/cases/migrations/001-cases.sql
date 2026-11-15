CREATE TABLE rule_cases_cases(
  id bigserial PRIMARY KEY,
  tenant_id bigint NOT NULL,
  company_id bigint NOT NULL,
  title text NOT NULL,
  priority text NOT NULL CHECK (priority IN ('low', 'normal', 'urgent')),
  amount numeric(20,2) NOT NULL,
  margin numeric(20,2),
  queue text,
  routed integer NOT NULL DEFAULT 0,
  flags integer NOT NULL DEFAULT 0,
  created_at timestamptz NOT NULL DEFAULT now(),
  version integer NOT NULL DEFAULT 1,
  FOREIGN KEY (tenant_id, company_id) REFERENCES companies(tenant_id, id)
);
