-- A tenant's public site (ADR 34): its address, title, logo, whether it is
-- on, and the hostnames that name it. The public listener finds the tenant
-- of a request by its hostname alone, so a hostname is unique across the
-- installation. One site per tenant; off until staff turn it on.
CREATE TABLE public_sites (
 tenant_id bigint PRIMARY KEY REFERENCES tenants(id),
 address text NOT NULL CHECK (address ~ '^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$'),
 title text NOT NULL CHECK (length(btrim(title)) BETWEEN 1 AND 120),
 enabled boolean NOT NULL DEFAULT false,
 -- A public image in Media; Media's tables are another module's, so no key.
 logo_asset_id bigint,
 version bigint NOT NULL DEFAULT 1,
 created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX public_sites_address_idx ON public_sites(address);

-- Lowercase, without a port. One hostname of a site is its primary one: the
-- address emailed links use.
CREATE TABLE public_site_hostnames (
 hostname text PRIMARY KEY CHECK (hostname = lower(hostname) AND length(hostname) BETWEEN 1 AND 253),
 tenant_id bigint NOT NULL REFERENCES public_sites(tenant_id) ON DELETE CASCADE,
 is_primary boolean NOT NULL DEFAULT false,
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX public_site_hostnames_primary_idx ON public_site_hostnames(tenant_id) WHERE is_primary;
CREATE INDEX public_site_hostnames_tenant_idx ON public_site_hostnames(tenant_id);
