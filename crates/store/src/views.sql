-- The views other programs read beacon.db through (spec/config.md, "Reading the database"). The
-- tables are internal and change between versions; these don't. A view only gains columns: one
-- that must lose or change a column becomes items_v2 beside this one. Made again at every start,
-- so adding a column needs no schema version. Times are milliseconds since 1970 UTC.
--
-- DuckDB's SQLite reader doesn't ask SQLite for a view's rows: it reads the view's SQL and runs
-- it itself over the tables. So everything here must mean the same in both. An integer is CAST
-- AS BIGINT (SQLite takes it as an integer; in DuckDB INTEGER is 32 bits and a time overflows
-- it), and a label is read with ->> (text in both; json_extract gives DuckDB quoted JSON).
DROP VIEW IF EXISTS items_v1;
CREATE VIEW items_v1 AS SELECT
    CAST(i.id AS BIGINT) AS id,
    CAST(i.source AS TEXT) AS source,
    CAST(i.source_name AS TEXT) AS source_name,
    CAST(i.kind AS TEXT) AS kind,
    CAST(i.key AS TEXT) AS key,
    CAST(i.class AS TEXT) AS class,
    CAST(i.state AS TEXT) AS state,
    CAST(i.push_level AS TEXT) AS push_level,
    CAST(i.push_status AS TEXT) AS push_status,
    CAST(i.push_reason AS TEXT) AS push_reason,
    CAST(i.title AS TEXT) AS title,
    CAST(i.body AS TEXT) AS body,
    CAST(i.url AS TEXT) AS url,
    CAST(i.context AS TEXT) AS context,
    CAST(i.labels AS TEXT) AS labels,
    CAST(i.labels ->> '$.service' AS TEXT) AS service,
    CAST(i.seen AS BIGINT) AS seen,
    CAST(i.created AS BIGINT) AS created,
    CAST(i.updated AS BIGINT) AS updated,
    CAST(i.opened AS BIGINT) AS opened,
    CAST(i.acknowledged AS BIGINT) AS acknowledged,
    CAST(i.dismissed AS BIGINT) AS dismissed,
    CAST(i.snoozed_until AS BIGINT) AS snoozed_until,
    CAST(i.done AS BIGINT) AS done,
    CAST(i.resolved AS BIGINT) AS resolved
FROM items i;
DROP VIEW IF EXISTS history_v1;
CREATE VIEW history_v1 AS SELECT
    CAST(h.id AS BIGINT) AS id,
    CAST(h.item AS BIGINT) AS item,
    CAST(h.at AS BIGINT) AS at,
    CAST(h.what AS TEXT) AS what,
    CAST(h.client AS TEXT) AS client,
    CAST(i.source AS TEXT) AS source,
    CAST(i.kind AS TEXT) AS kind,
    CAST(i.class AS TEXT) AS class,
    CAST(i.labels ->> '$.service' AS TEXT) AS service,
    CAST(h.state AS TEXT) AS state,
    CAST(h.title AS TEXT) AS title,
    CAST(h.push_level AS TEXT) AS push_level,
    CAST(h.push_status AS TEXT) AS push_status,
    CAST(h.push_reason AS TEXT) AS push_reason
FROM history h JOIN items i ON i.id = h.item;
