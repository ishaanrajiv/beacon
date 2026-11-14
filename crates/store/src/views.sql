-- The views other programs read beacon.db through (spec/config.md, "Reading the database"). The
-- tables are internal and change between versions; these don't. A view only gains columns: one
-- that must lose or change a column becomes items_v2 beside this one. Made again at every start,
-- so adding a column needs no schema version. Every column is a plain column or a CAST, so SQLite
-- reports its type (INTEGER or TEXT) and DuckDB's SQLite reader types it. Times are milliseconds
-- since 1970 UTC.
DROP VIEW IF EXISTS items_v1;
CREATE VIEW items_v1 AS SELECT
    CAST(i.id AS INTEGER) AS id,
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
    CAST(json_extract(i.labels, '$.service') AS TEXT) AS service,
    CAST(i.seen AS INTEGER) AS seen,
    CAST(i.created AS INTEGER) AS created,
    CAST(i.updated AS INTEGER) AS updated,
    CAST(i.opened AS INTEGER) AS opened,
    CAST(i.acknowledged AS INTEGER) AS acknowledged,
    CAST(i.dismissed AS INTEGER) AS dismissed,
    CAST(i.snoozed_until AS INTEGER) AS snoozed_until,
    CAST(i.done AS INTEGER) AS done,
    CAST(i.resolved AS INTEGER) AS resolved
FROM items i;
DROP VIEW IF EXISTS history_v1;
CREATE VIEW history_v1 AS SELECT
    CAST(h.id AS INTEGER) AS id,
    CAST(h.item AS INTEGER) AS item,
    CAST(h.at AS INTEGER) AS at,
    CAST(h.what AS TEXT) AS what,
    CAST(h.client AS TEXT) AS client,
    CAST(i.source AS TEXT) AS source,
    CAST(i.kind AS TEXT) AS kind,
    CAST(i.class AS TEXT) AS class,
    CAST(json_extract(i.labels, '$.service') AS TEXT) AS service,
    CAST(h.state AS TEXT) AS state,
    CAST(h.title AS TEXT) AS title,
    CAST(h.push_level AS TEXT) AS push_level,
    CAST(h.push_status AS TEXT) AS push_status,
    CAST(h.push_reason AS TEXT) AS push_reason
FROM history h JOIN items i ON i.id = h.item;
