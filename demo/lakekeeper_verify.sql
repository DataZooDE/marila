-- REQUIREMENTS.md §9 step 5: "observe full CREATE/INSERT/UPDATE/DELETE
-- working against RustFS via Lakekeeper" through marila's
-- /iceberg/v1/* reverse-proxy.
--
-- Usage:
--   docker compose --profile lakekeeper up -d
--   cargo run -p marila &
--   echo "127.0.0.1 rustfs" | sudo tee -a /etc/hosts   # per CLAUDE.md D-2
--   duckdb < demo/lakekeeper_verify.sql
--
-- The `rustfs` hostname mapping is required because Lakekeeper returns
-- the warehouse storage URL as `http://rustfs:9000/...` (the docker
-- network alias), and DuckDB-iceberg matches secrets by URL prefix.
-- Without the /etc/hosts entry the data-file PUT goes out unsigned
-- and RustFS returns 403 (doc/DISCOVERIES.md D-2).

INSTALL iceberg;
LOAD iceberg;

-- S3 credentials marila + Lakekeeper share. The ENDPOINT below tells
-- DuckDB *where* to talk to S3 — we use localhost:9000 because this
-- demo runs on the host and RustFS exposes its port there.
--
-- Lakekeeper itself (inside docker) writes metadata via the
-- `http://rustfs:9000` alias from the storage-profile, but the secret
-- below routes DuckDB's host-side data I/O to localhost. The two views
-- of the same RustFS instance line up because the URL prefix matching
-- happens on the *bucket name* (SCOPE), not the host.
CREATE OR REPLACE SECRET s3_warehouse (
    TYPE s3,
    PROVIDER config,
    KEY_ID 'marila',
    SECRET 'marilasecret',
    ENDPOINT 'localhost:9000',
    REGION 'eu-west-1',
    URL_STYLE 'path',
    USE_SSL false,
    SCOPE 's3://marila-warehouse/'
);

-- ATTACH the marila-warehouse Lakekeeper exposes via marila's pass-through.
-- - ACCESS_DELEGATION_MODE 'none' so DuckDB-iceberg uses the TYPE s3
--   secret above for data writes instead of asking the catalog for
--   vended credentials (D-1; duckdb-iceberg#594).
-- - AUTHORIZATION_TYPE 'none' because Lakekeeper runs with the
--   allow-all authz backend in this dev compose; the default 'oauth2'
--   would try to fetch a token from an empty URL and fail.
-- Note: no `SECRET` clause on ATTACH — that option is only valid for
-- the OAuth2/SigV4 catalog auth flows. With AUTHORIZATION_TYPE 'none'
-- the `s3_warehouse` secret above is selected by URL prefix (SCOPE)
-- when DuckDB does data-file I/O against the warehouse bucket.
ATTACH 'marila-warehouse' AS lake (
    TYPE iceberg,
    ENDPOINT 'http://localhost:8080/iceberg',
    AUTHORIZATION_TYPE 'none',
    ACCESS_DELEGATION_MODE 'none'
);

-- Namespace = Iceberg schema. Ensure it exists first — `DROP TABLE
-- IF EXISTS lake.demo.orders` would otherwise error on a fresh run
-- because referencing a non-existent schema bubbles up as a Catalog
-- error, not a silently-skipped IF-EXISTS.
CREATE SCHEMA IF NOT EXISTS lake.demo;
DROP TABLE IF EXISTS lake.demo.orders;

-- Tabular round-trip: CREATE → INSERT → SELECT → UPDATE → DELETE.
-- The table is unpartitioned/unsorted so DuckDB-iceberg's UPDATE/DELETE
-- paths apply (doc/DISCOVERIES.md D-5).
DROP TABLE IF EXISTS lake.demo.orders;
CREATE TABLE lake.demo.orders (
    id           INT,
    customer     VARCHAR,
    amount_cents BIGINT
);

INSERT INTO lake.demo.orders VALUES
    (1, 'alice', 1000),
    (2, 'bob',   2000),
    (3, 'carol', 3000);

SELECT 'after INSERT', count(*) AS rows FROM lake.demo.orders;
SELECT * FROM lake.demo.orders ORDER BY id;

-- Iceberg merge-on-read UPDATE (D-5: copy-on-write isn't supported).
UPDATE lake.demo.orders SET amount_cents = 1500 WHERE id = 1;
SELECT 'after UPDATE id=1', amount_cents FROM lake.demo.orders WHERE id = 1;

-- Iceberg merge-on-read DELETE.
DELETE FROM lake.demo.orders WHERE customer = 'bob';
SELECT 'after DELETE', count(*) AS rows FROM lake.demo.orders;
SELECT * FROM lake.demo.orders ORDER BY id;

-- Cleanup leaves the warehouse intact (marila-warehouse is shared by
-- other demos and tests). Drop the table individually since
-- DROP SCHEMA CASCADE isn't supported on Iceberg schemas yet (D-5).
DROP TABLE lake.demo.orders;
DROP SCHEMA lake.demo;
