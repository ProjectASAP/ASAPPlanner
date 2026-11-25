# BGP analytics SQL corpus (ClickHouse dialect).
#
# Source: 15 real production ClickHouse queries against a BGP-update /
# RIB-snapshot dataset (user-provided, 2026-07-21) -- prefix update/withdrawal
# top-k, AS-path parsing for origin-ASN attribution, MOAS (multi-origin-AS)
# detection, prefix deaggregation, per-peer RIB visibility and churn
# classification between two snapshots.
#
# Schema (as used by the queries):
#   bgp_updates(timestamp, collector, peer_ip, peer_asn, prefix, operation, as_path)
#   bgp_rib_state(snapshot_ts, collector, peer_ip, peer_asn, prefix)
# Both are referenced with and without a "bgp." schema qualifier in the
# original queries -- both forms are preserved verbatim below.
#
# ClickHouse's `{name:Type}` query parameters are not valid SQL syntax for any
# parser (curly-brace tokens), so each has been substituted with a concrete
# literal of the declared type. Nothing else about the queries was changed:
# `count()`, `FORMAT Null`, `x -> ...` array-lambda arguments, `arr[-1]`
# indexing, and ClickHouse-only builtins (`uniqExact`, `countIf`,
# `toStartOfInterval`, `lagInFrame`, `isIPAddressInRange`, `multiIf`,
# `arrayJoin`, `groupUniqArray`, `ifNull`) are all preserved as given.
#
# Parsed under `SqlDialect::ClickhouseSQL` (sqlparser's vendored
# `ClickHouseDialect`). That dialect switch is what this corpus pins -- it is
# NOT full ClickHouse conformance. Two categories of failure remain, both
# clean errors (never panics):
#   - Unknown function at the DataFusion planning stage: the ClickHouse
#     builtins above have no DataFusion equivalent registered. Queries 3-13
#     (except where noted) fail here.
#   - Unsupported grammar at the sqlparser parsing stage: query 14 uses
#     ClickHouse's scalar/tuple `WITH <expr> AS <alias>` binding (distinct
#     from an ordinary `WITH name AS (subquery)` CTE); query 15 uses
#     ClickHouse's shorthand `USING <col>` without parens. Neither construct
#     is implemented by the vendored sqlparser version.
# Only queries 1-2 (plain COUNT/GROUP BY/ORDER BY/LIMIT, no ClickHouse-only
# functions) lower end to end today.

-- 1. Update-count top-k per prefix.
SELECT
    prefix,
    count() AS update_count
FROM bgp.bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp <  '2024-01-01 01:00:00'
  AND collector IN ('rrc00', 'route-views2')
GROUP BY prefix
ORDER BY update_count DESC, prefix ASC
LIMIT 10
FORMAT Null;

-- 2. Withdrawal-count top-k per prefix.
SELECT
    prefix,
    count() AS withdrawal_count
FROM bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp < '2024-01-01 01:00:00'
  AND operation = 'W'
  AND collector IN ('rrc00', 'route-views2')
GROUP BY prefix
ORDER BY withdrawal_count DESC, prefix ASC
LIMIT 10;

-- 3. Origin-ASN announcement counts (AS-path parsed via arrayFilter lambda).
SELECT
    as_path_array[-1] AS origin_asn,
    count() AS announcement_count
FROM
(
    SELECT
        arrayFilter(
            x -> match(x, '^[0-9]+$'),
            splitByWhitespace(as_path)
        ) AS as_path_array
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
WHERE length(as_path_array) > 0
GROUP BY origin_asn
ORDER BY announcement_count DESC, origin_asn ASC
LIMIT 10
FORMAT Null;

-- 4. Distinct prefixes originated by a given ASN.
SELECT DISTINCT
    prefix
FROM
(
    SELECT
        prefix,
        arrayFilter(
            x -> match(x, '^[0-9]+$'),
            splitByWhitespace(as_path)
        ) AS as_path_array
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
WHERE length(as_path_array) > 0
  AND as_path_array[-1] = '65000'
ORDER BY prefix
FORMAT Null;

-- 5. Distinct prefixes per origin ASN.
SELECT
    as_path_array[-1] AS origin_asn,
    uniqExact(prefix) AS distinct_prefixes
FROM
(
    SELECT
        prefix,
        arrayFilter(
            x -> match(x, '^[0-9]+$'),
            splitByWhitespace(as_path)
        ) AS as_path_array
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
WHERE length(as_path_array) > 0
GROUP BY origin_asn
ORDER BY distinct_prefixes DESC, origin_asn ASC
LIMIT 10
FORMAT Null;

-- 6. Per-peer update counts and distinct prefixes.
SELECT
    collector,
    peer_ip,
    peer_asn,
    count() AS update_count,
    uniqExact(prefix) AS distinct_prefixes
FROM bgp.bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp <  '2024-01-01 01:00:00'
  AND collector = 'rrc00'
GROUP BY
    collector,
    peer_ip,
    peer_asn
ORDER BY update_count DESC
FORMAT Null;

-- 7. Announcement/withdrawal counts bucketed by time interval.
SELECT
    toStartOfInterval(
        timestamp,
        toIntervalMinute(5)
    ) AS bucket,
    countIf(operation = 'A') AS announcements,
    countIf(operation = 'W') AS withdrawals
FROM bgp.bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp <  '2024-01-01 01:00:00'
  AND collector = 'rrc00'
GROUP BY bucket
ORDER BY bucket
FORMAT Null;

-- 8. MOAS (multi-origin-AS) detection.
SELECT
    prefix,
    uniqExact(as_path_array[-1]) AS origin_count,
    groupUniqArray(as_path_array[-1]) AS origins
FROM
(
    SELECT
        prefix,
        arrayFilter(
            x -> match(x, '^[0-9]+$'),
            splitByWhitespace(as_path)
        ) AS as_path_array
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
WHERE length(as_path_array) > 0
GROUP BY prefix
HAVING origin_count > 1
ORDER BY origin_count DESC, prefix ASC
FORMAT Null;

-- 9. More-specific prefixes within an aggregate (prefix deaggregation).
SELECT DISTINCT
    prefix
FROM bgp.bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp <  '2024-01-01 01:00:00'
  AND collector = 'rrc00'
  AND operation = 'A'
  AND prefix != '10.0.0.0/8'
  AND isIPAddressInRange(
        splitByChar('/', prefix)[1],
        '10.0.0.0/8'
      )
  AND toUInt16OrZero(splitByChar('/', prefix)[2])
      > toUInt16OrZero(
          splitByChar('/', '10.0.0.0/8')[2]
        )
ORDER BY prefix
FORMAT Null;

-- 10. Per-prefix announcement timeline with origin ASN.
SELECT
    timestamp,
    collector,
    peer_ip,
    as_path_array[-1] AS origin_asn
FROM
(
    SELECT
        timestamp,
        collector,
        peer_ip,
        arrayFilter(
            x -> match(x, '^[0-9]+$'),
            splitByWhitespace(as_path)
        ) AS as_path_array
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND prefix = '10.0.0.0/24'
      AND operation = 'A'
      AND as_path != ''
)
WHERE length(as_path_array) > 0
ORDER BY timestamp, collector, peer_ip
FORMAT Null;

-- 11. Path-change count per prefix via a lag window function.
WITH ordered AS
(
    SELECT
        prefix,
        collector,
        peer_ip,
        timestamp,
        as_path,
        lagInFrame(as_path) OVER
        (
            PARTITION BY prefix, collector, peer_ip
            ORDER BY timestamp
            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
        ) AS previous_path
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
SELECT
    prefix,
    countIf(previous_path != '' AND previous_path != as_path) AS path_changes
FROM ordered
GROUP BY prefix
ORDER BY path_changes DESC, prefix ASC
LIMIT 10
FORMAT Null;

-- 12. Distinct AS-path count per prefix (path diversity).
SELECT
    prefix,
    uniqExact(as_path) AS distinct_paths
FROM bgp.bgp_updates
WHERE timestamp >= '2024-01-01 00:00:00'
  AND timestamp <  '2024-01-01 01:00:00'
  AND collector = 'rrc00'
  AND operation = 'A'
  AND as_path != ''
GROUP BY prefix
ORDER BY distinct_paths DESC, prefix ASC
LIMIT 10;

-- 13. ASN occurrence counts across all paths (not just origin).
SELECT
    asn,
    count() AS occurrence_count
FROM
(
    SELECT
        arrayJoin(
            arrayFilter(
                x -> match(x, '^[0-9]+$'),
                splitByWhitespace(as_path)
            )
        ) AS asn
    FROM bgp.bgp_updates
    WHERE timestamp >= '2024-01-01 00:00:00'
      AND timestamp <  '2024-01-01 01:00:00'
      AND collector = 'rrc00'
      AND operation = 'A'
      AND as_path != ''
)
GROUP BY asn
ORDER BY occurrence_count DESC, asn ASC
LIMIT 10
FORMAT Null;

-- 14. RIB visibility fraction for a set of target prefixes. Uses ClickHouse's
-- scalar/tuple `WITH <expr> AS <alias>` binding mixed with ordinary CTEs --
-- not representable by the vendored sqlparser grammar (parse failure, not a
-- function gap).
WITH
    ('10.0.0.0/24', '10.0.1.0/24') AS target_prefixes,
    arrayExists(
        x -> position(x, ':') > 0,
        target_prefixes
    ) AS target_is_ipv6,

active_peers AS
(
    SELECT DISTINCT
        collector,
        peer_ip,
        peer_asn
    FROM bgp.bgp_rib_state
    WHERE snapshot_ts >= '2024-01-01 00:00:00'
      AND snapshot_ts <  '2024-01-01 01:00:00'
      AND collector IN ('rrc00', 'route-views2')
      AND (position(prefix, ':') > 0) = target_is_ipv6
),

visible_peers AS
(
    SELECT DISTINCT
        collector,
        peer_ip,
        peer_asn
    FROM bgp.bgp_rib_state
    WHERE snapshot_ts >= '2024-01-01 00:00:00'
      AND snapshot_ts <  '2024-01-01 01:00:00'
      AND collector IN ('rrc00', 'route-views2')
      AND prefix IN target_prefixes
)

SELECT
    (
        SELECT count()
        FROM visible_peers
    ) AS visible_peer_count,
    (
        SELECT count()
        FROM active_peers
    ) AS active_peer_count,
    visible_peer_count /
        nullIf(active_peer_count, 0) AS visibility_fraction
FORMAT Null;

-- 15. RIB churn classification between two snapshots. Uses ClickHouse's
-- shorthand `USING <col>` (no parens) -- not representable by the vendored
-- sqlparser grammar (parse failure, not a function gap).
WITH
peer_counts AS
(
    SELECT
        peer_ip,
        peer_asn,
        countIf(snapshot_ts = '2024-01-01 00:00:00') AS routes_before,
        countIf(snapshot_ts = '2024-01-01 01:00:00') AS routes_after,
        uniqExact(snapshot_ts) AS snapshot_count
    FROM bgp.bgp_rib_state
    WHERE snapshot_ts IN ('2024-01-01 00:00:00', '2024-01-01 01:00:00')
      AND collector = 'rrc00'
    GROUP BY
        peer_ip,
        peer_asn
),

stable_peers AS
(
    SELECT
        peer_ip,
        peer_asn
    FROM peer_counts
    WHERE snapshot_count = 2
      AND routes_before > 0
      AND routes_after > 0
      AND routes_after / routes_before BETWEEN 0.5 AND 2.0
),

before_state AS
(
    SELECT
        r.prefix,
        uniqExact((r.peer_ip, r.peer_asn)) AS peers_before
    FROM bgp.bgp_rib_state AS r
    INNER JOIN stable_peers AS s
        ON r.peer_ip = s.peer_ip
       AND r.peer_asn = s.peer_asn
    WHERE r.snapshot_ts = '2024-01-01 00:00:00'
      AND r.collector = 'rrc00'
    GROUP BY r.prefix
),

after_state AS
(
    SELECT
        r.prefix,
        uniqExact((r.peer_ip, r.peer_asn)) AS peers_after
    FROM bgp.bgp_rib_state AS r
    INNER JOIN stable_peers AS s
        ON r.peer_ip = s.peer_ip
       AND r.peer_asn = s.peer_asn
    WHERE r.snapshot_ts = '2024-01-01 01:00:00'
      AND r.collector = 'rrc00'
    GROUP BY r.prefix
),

changes AS
(
    SELECT
        coalesce(
            before_state.prefix,
            after_state.prefix
        ) AS route_prefix,
        ifNull(peers_before, 0) AS peers_before,
        ifNull(peers_after, 0) AS peers_after,
        multiIf(
            peers_before = 0 AND peers_after > 0,
                'newly_visible',
            peers_before > 0 AND peers_after = 0,
                'disappeared',
            peers_after > peers_before,
                'increased_visibility',
            peers_after < peers_before,
                'decreased_visibility',
            'unchanged'
        ) AS change_type
    FROM before_state
    FULL OUTER JOIN after_state USING prefix
)

SELECT
    change_type,
    count() AS prefix_count
FROM changes
GROUP BY change_type
FORMAT Null;
