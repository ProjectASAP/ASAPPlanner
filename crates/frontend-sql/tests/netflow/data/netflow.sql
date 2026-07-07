# Netflow SQL corpus.
#
# Schema in the test:
#   netflow_table(time, srcip, dstip, srcport, dstport, proto, pkt_len)
#
# The controller SQL front end keeps simple table predicates on Scan nodes; the
# test asserts the aggregate intent shape around those time predicates.

-- 1. Temporal quantile grouped by source IP.
SELECT srcip, approx_percentile_cont(pkt_len, 0.95) AS p95_pkt_len
FROM netflow_table
WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
  AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
GROUP BY srcip;

-- 2. Temporal quantile grouped by destination IP.
SELECT dstip, approx_percentile_cont(pkt_len, 0.95) AS p95_pkt_len
FROM netflow_table
WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
  AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
GROUP BY dstip;

-- 3. Top-k by event count.
SELECT srcip, COUNT(pkt_len) AS transfer_events
FROM netflow_table
WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
  AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
GROUP BY srcip
ORDER BY transfer_events DESC
LIMIT 10;

-- 4. Top-k by sum. The L3 front end keeps this as SUM plus Sort/Limit today.
SELECT dstip, SUM(pkt_len) AS bytes
FROM netflow_table
WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
  AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
GROUP BY dstip
ORDER BY bytes DESC
LIMIT 10;

-- 5. Cardinality with sort/limit post-processing.
SELECT srcip, COUNT(DISTINCT dstip) AS unique_peers
FROM netflow_table
WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
  AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
GROUP BY srcip
ORDER BY unique_peers DESC
LIMIT 20;

-- 6. MinMax family query.
SELECT srcip, MAX(pkt_len) AS max_pkt_len
FROM netflow_table
WHERE time BETWEEN CAST('2018-05-17 13:10:00' AS TIMESTAMP)
               AND CAST('2018-05-17 13:10:01' AS TIMESTAMP)
GROUP BY srcip
ORDER BY max_pkt_len DESC
LIMIT 10;

-- 7. Parser-supported nested aggregate shape.
SELECT srcip, MAX(result) AS max_pair_bytes
FROM (
  SELECT srcip, dstip, SUM(pkt_len) AS result
  FROM netflow_table
  WHERE time >= CAST('2018-05-17 13:10:01' AS TIMESTAMP)
    AND time <  CAST('2018-05-17 13:10:11' AS TIMESTAMP)
  GROUP BY srcip, dstip
) pair_bytes
GROUP BY srcip;
