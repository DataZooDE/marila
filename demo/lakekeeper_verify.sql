-- Standalone Iceberg-via-DuckDB smoke test that does NOT depend on the
-- Python demos. Useful for verifying the /iceberg/v1/* reverse-proxy
-- end-to-end with a single command:
--
--   docker compose --profile lakekeeper up -d && cargo run -p marila &
--   duckdb < demo/lakekeeper_verify.sql
--
-- For the realistic 1,000-row analytical narrative (top-region revenue,
-- bad-row cleanup, category rebrand) run `demo/demo_tables.py` instead;
-- this script is intentionally minimal.

INSTALL iceberg;
LOAD iceberg;

-- S3 secret: routes DuckDB's data I/O to localhost:9000 because this
-- script runs on the host. Lakekeeper itself writes via the docker
-- alias `rustfs:9000` — same RustFS instance, two hostnames.
CREATE OR REPLACE SECRET s3_warehouse (
    TYPE s3, PROVIDER config,
    KEY_ID 'marila', SECRET 'marilasecret',
    ENDPOINT 'localhost:9000',
    REGION 'eu-west-1', URL_STYLE 'path', USE_SSL false,
    SCOPE 's3://marila-warehouse/'
);

-- ATTACH the bootstrap `marila-warehouse` Lakekeeper creates on first
-- `docker compose up`. AUTHORIZATION_TYPE 'none' because Lakekeeper
-- runs in allow-all mode here; ACCESS_DELEGATION_MODE 'none' so
-- DuckDB-iceberg uses the secret above for data writes (D-1).
ATTACH 'marila-warehouse' AS lake (
    TYPE iceberg,
    ENDPOINT 'http://localhost:8080/iceberg',
    AUTHORIZATION_TYPE 'none',
    ACCESS_DELEGATION_MODE 'none'
);

-- Smoke test: full lifecycle on a tiny table.
CREATE SCHEMA IF NOT EXISTS lake.smoke;
DROP TABLE IF EXISTS lake.smoke.heartbeat;
CREATE TABLE lake.smoke.heartbeat (id INT, beat VARCHAR);

INSERT INTO lake.smoke.heartbeat VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');
SELECT 'after INSERT' AS step, count(*) AS rows FROM lake.smoke.heartbeat;
SELECT * FROM lake.smoke.heartbeat ORDER BY id;

UPDATE lake.smoke.heartbeat SET beat = 'updated' WHERE id = 2;
SELECT 'after UPDATE' AS step, beat FROM lake.smoke.heartbeat WHERE id = 2;

DELETE FROM lake.smoke.heartbeat WHERE id = 3;
SELECT 'after DELETE' AS step, count(*) AS rows FROM lake.smoke.heartbeat;

-- Cleanup. DROP SCHEMA CASCADE isn't supported on Iceberg schemas yet
-- (D-5), so drop the table individually before the schema.
DROP TABLE lake.smoke.heartbeat;
DROP SCHEMA lake.smoke;
