-- Operator init for the traces test stack. The application migration does not
-- create or drop this extension. Production USAGE and EXECUTE grants are a
-- later operator step; this file only installs the reviewed objects.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mega2_traces_ext_owner'
    ) THEN
        CREATE ROLE mega2_traces_ext_owner NOLOGIN;
    END IF;
END
$$;

CREATE SCHEMA IF NOT EXISTS mega2_extensions AUTHORIZATION mega2_traces_ext_owner;

REVOKE CREATE ON SCHEMA mega2_extensions FROM PUBLIC;

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA mega2_extensions VERSION '1.6';
