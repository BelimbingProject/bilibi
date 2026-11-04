CREATE SEQUENCE core_product_codes;
CREATE TABLE core_products (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    code text NOT NULL CHECK (length(code) BETWEEN 1 AND 64),
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 200),
    kind text NOT NULL CHECK (kind IN ('goods', 'service')),
    base_unit text NOT NULL CHECK (length(base_unit) BETWEEN 1 AND 80),
    category text CHECK (length(category) BETWEEN 1 AND 120),
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')),
    version bigint NOT NULL DEFAULT 1,
    UNIQUE (tenant_id, code),
    UNIQUE (tenant_id, id)
);
CREATE INDEX core_products_list ON core_products(tenant_id, name, id);
CREATE TABLE core_product_prices (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id bigint NOT NULL REFERENCES tenants(id),
    product_id bigint NOT NULL,
    selling_company bigint NOT NULL,
    currency text NOT NULL,
    effective_on date NOT NULL,
    amount numeric NOT NULL CHECK (amount >= 0 AND amount < 1000000000000000),
    FOREIGN KEY (tenant_id, product_id) REFERENCES core_products(tenant_id, id),
    UNIQUE (tenant_id, product_id, selling_company, currency, effective_on)
);
