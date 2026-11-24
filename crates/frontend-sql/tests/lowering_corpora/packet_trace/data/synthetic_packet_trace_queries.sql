# Synthetic Packet Trace Evaluation Queries (DQC)
#
# Every evaluation query in the DQC `metrics.py` synthetic-trace fidelity
# benchmark, grouped as `eval_metrics` does: packet level (29), flow level
# stateless (21), flow level stateful (20) = 70 queries.
#
# Each query is run on the real trace and the synthetic trace; the two result
# sets are compared with a distance metric to score fidelity. Only the queries
# are listed here (not the distance metrics). The `_topnvalue` / `_topnkey` /
# `_distribution` code variants are the same query scored differently, so each
# is listed once.
#
# Schema: packets(srcip, dstip, srcport, dstport, proto, time, pkt_len).
# flow / 5-tuple = (srcip, dstip, srcport, dstport, proto).
#
# Statements are ';'-terminated; '#' / '--' lines are comments.

-- ========================================================================
-- 1. Packet Level
-- ========================================================================

-- 1a. Global packet statistics

-- 1. P-COUNT — Total number of packets.
SELECT COUNT(*) AS total_packets FROM packets;

-- 2. P-SRCIP-CD — Number of distinct source IPs.
SELECT COUNT(DISTINCT srcip) AS n_src_ips FROM packets;

-- 3. P-SRCIP-DIST — Number of packets sent by each source IP.
SELECT srcip, COUNT(*) AS pkts FROM packets GROUP BY srcip ORDER BY pkts DESC;

-- 4. P-DSTIP-CD — Number of distinct destination IPs.
SELECT COUNT(DISTINCT dstip) AS n_dst_ips FROM packets;

-- 5. P-DSTIP-DIST — Number of packets received by each destination IP.
SELECT dstip, COUNT(*) AS pkts FROM packets GROUP BY dstip ORDER BY pkts DESC;

-- 6. P-SRCPORT-CD — Number of distinct source ports.
SELECT COUNT(DISTINCT srcport) AS n_src_ports FROM packets;

-- 7. P-SRCPORT-DIST — Number of packets per source port.
SELECT srcport, COUNT(*) AS pkts FROM packets GROUP BY srcport;

-- 8. P-DSTPORT-CD — Number of distinct destination ports.
SELECT COUNT(DISTINCT dstport) AS n_dst_ports FROM packets;

-- 9. P-DSTPORT-DIST — Number of packets per destination port.
SELECT dstport, COUNT(*) AS pkts FROM packets GROUP BY dstport;

-- 10. P-PROTO-CD — Number of distinct protocols.
SELECT COUNT(DISTINCT proto) AS n_protocols FROM packets;

-- 11. P-PROTO-DIST — Number of packets per protocol.
SELECT proto, COUNT(*) AS pkts FROM packets GROUP BY proto;

-- 12. P-TIME-DIST — Packet timestamps.
SELECT time FROM packets;

-- 13. P-LEN-SUM — Total bytes (sum of packet lengths).
SELECT SUM(pkt_len) AS total_bytes FROM packets;

-- 14. P-LEN-AVG — Average packet length.
SELECT AVG(pkt_len) AS avg_pkt_len FROM packets;

-- 15. P-LEN-DIST — Packet lengths.
SELECT pkt_len FROM packets;

-- 1b. Per source-port aggregations

-- 16. SP-PKT — Number of packets per source port.
SELECT srcport, COUNT(*) AS pkts FROM packets GROUP BY srcport ORDER BY pkts DESC;

-- 17. SP-BYTE — Total bytes per source port.
SELECT srcport, SUM(pkt_len) AS bytes FROM packets GROUP BY srcport ORDER BY bytes DESC;

-- 18. SP-CD-SRCIP — Distinct source IPs per source port.
SELECT srcport, COUNT(DISTINCT srcip) AS n FROM packets GROUP BY srcport ORDER BY n DESC;

-- 19. SP-CD-DSTIP — Distinct destination IPs per source port.
SELECT srcport, COUNT(DISTINCT dstip) AS n FROM packets GROUP BY srcport ORDER BY n DESC;

-- 20. SP-CD-DSTPORT — Distinct destination ports per source port.
SELECT srcport, COUNT(DISTINCT dstport) AS n FROM packets GROUP BY srcport ORDER BY n DESC;

-- 21. SP-CD-DSTIPPORT — Distinct (dstip, dstport) pairs per source port.
SELECT srcport, COUNT(DISTINCT dstip, dstport) AS n FROM packets GROUP BY srcport ORDER BY n DESC;

-- 22. SP-CD-FLOW — Distinct flows (5-tuples) per source port.
SELECT srcport, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n
FROM packets GROUP BY srcport ORDER BY n DESC;

-- 1c. Per destination-port aggregations

-- 23. DP-PKT — Number of packets per destination port.
SELECT dstport, COUNT(*) AS pkts FROM packets GROUP BY dstport ORDER BY pkts DESC;

-- 24. DP-BYTE — Total bytes per destination port.
SELECT dstport, SUM(pkt_len) AS bytes FROM packets GROUP BY dstport ORDER BY bytes DESC;

-- 25. DP-CD-DSTIP — Distinct destination IPs per destination port.
SELECT dstport, COUNT(DISTINCT dstip) AS n FROM packets GROUP BY dstport ORDER BY n DESC;

-- 26. DP-CD-SRCIP — Distinct source IPs per destination port.
SELECT dstport, COUNT(DISTINCT srcip) AS n FROM packets GROUP BY dstport ORDER BY n DESC;

-- 27. DP-CD-SRCPORT — Distinct source ports per destination port.
SELECT dstport, COUNT(DISTINCT srcport) AS n FROM packets GROUP BY dstport ORDER BY n DESC;

-- 28. DP-CD-SRCIPPORT — Distinct (srcip, srcport) pairs per destination port.
SELECT dstport, COUNT(DISTINCT srcip, srcport) AS n FROM packets GROUP BY dstport ORDER BY n DESC;

-- 29. DP-CD-FLOW — Distinct flows (5-tuples) per destination port.
SELECT dstport, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n
FROM packets GROUP BY dstport ORDER BY n DESC;

-- ========================================================================
-- 2. Flow Level Stateless
-- ========================================================================

-- 2a. Per source IP

-- 30. SI-PKT — Number of packets per source IP.
SELECT srcip, COUNT(*) AS pkts FROM packets GROUP BY srcip ORDER BY pkts DESC;

-- 31. SI-BYTE — Total bytes per source IP.
SELECT srcip, SUM(pkt_len) AS bytes FROM packets GROUP BY srcip ORDER BY bytes DESC;

-- 32. SI-CD-SRCPORT — Distinct source ports per source IP.
SELECT srcip, COUNT(DISTINCT srcport) AS n FROM packets GROUP BY srcip ORDER BY n DESC;

-- 33. SI-CD-DSTIP — Distinct destination IPs contacted per source IP.
SELECT srcip, COUNT(DISTINCT dstip) AS n FROM packets GROUP BY srcip ORDER BY n DESC;

-- 34. SI-CD-DSTPORT — Distinct destination ports contacted per source IP.
SELECT srcip, COUNT(DISTINCT dstport) AS n FROM packets GROUP BY srcip ORDER BY n DESC;

-- 35. SI-CD-DSTIPPORT — Distinct (dstip, dstport) pairs contacted per source IP.
SELECT srcip, COUNT(DISTINCT dstip, dstport) AS n FROM packets GROUP BY srcip ORDER BY n DESC;

-- 36. SI-CD-FLOW — Distinct flows (5-tuples) per source IP.
SELECT srcip, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n
FROM packets GROUP BY srcip ORDER BY n DESC;

-- 2b. Per destination IP

-- 37. DI-PKT — Number of packets per destination IP.
SELECT dstip, COUNT(*) AS pkts FROM packets GROUP BY dstip ORDER BY pkts DESC;

-- 38. DI-BYTE — Total bytes per destination IP.
SELECT dstip, SUM(pkt_len) AS bytes FROM packets GROUP BY dstip ORDER BY bytes DESC;

-- 39. DI-CD-DSTPORT — Distinct destination ports per destination IP.
SELECT dstip, COUNT(DISTINCT dstport) AS n FROM packets GROUP BY dstip ORDER BY n DESC;

-- 40. DI-CD-SRCIP — Distinct source IPs contacting per destination IP.
SELECT dstip, COUNT(DISTINCT srcip) AS n FROM packets GROUP BY dstip ORDER BY n DESC;

-- 41. DI-CD-SRCPORT — Distinct source ports per destination IP.
SELECT dstip, COUNT(DISTINCT srcport) AS n FROM packets GROUP BY dstip ORDER BY n DESC;

-- 42. DI-CD-SRCIPPORT — Distinct (srcip, srcport) pairs per destination IP.
SELECT dstip, COUNT(DISTINCT srcip, srcport) AS n FROM packets GROUP BY dstip ORDER BY n DESC;

-- 43. DI-CD-FLOW — Distinct flows (5-tuples) per destination IP.
SELECT dstip, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n
FROM packets GROUP BY dstip ORDER BY n DESC;

-- 2c. Per IP pair (srcip, dstip)

-- 44. PR-PKT — Number of packets per source-destination IP pair.
SELECT srcip, dstip, COUNT(*) AS pkts FROM packets GROUP BY srcip, dstip ORDER BY pkts DESC;

-- 45. PR-BYTE — Total bytes per IP pair.
SELECT srcip, dstip, SUM(pkt_len) AS bytes FROM packets GROUP BY srcip, dstip ORDER BY bytes DESC;

-- 46. PR-CD-SRCPORT — Distinct source ports per IP pair.
SELECT srcip, dstip, COUNT(DISTINCT srcport) AS n FROM packets GROUP BY srcip, dstip ORDER BY n DESC;

-- 47. PR-CD-DSTPORT — Distinct destination ports per IP pair.
SELECT srcip, dstip, COUNT(DISTINCT dstport) AS n FROM packets GROUP BY srcip, dstip ORDER BY n DESC;

-- 48. PR-CD-FLOW — Distinct flows (5-tuples) per IP pair.
SELECT srcip, dstip, COUNT(DISTINCT srcip, dstip, srcport, dstport, proto) AS n
FROM packets GROUP BY srcip, dstip ORDER BY n DESC;

-- 2d. Per 5-tuple flow (srcip, dstip, srcport, dstport, proto)

-- 49. FT-PKT — Number of packets per 5-tuple flow.
SELECT srcip, dstip, srcport, dstport, proto, COUNT(*) AS pkts
FROM packets GROUP BY srcip, dstip, srcport, dstport, proto ORDER BY pkts DESC;

-- 50. FT-BYTE — Total bytes per 5-tuple flow.
SELECT srcip, dstip, srcport, dstport, proto, SUM(pkt_len) AS bytes
FROM packets GROUP BY srcip, dstip, srcport, dstport, proto ORDER BY bytes DESC;

-- ========================================================================
-- 3. Flow Level Stateful
-- gap = inter-arrival time between consecutive packets of an entity
-- (ordered by time).
-- ========================================================================

-- 3a. Per source IP

-- 51. SI-AINT — Average packet inter-arrival time per source IP (> 10 packets).
WITH gaps AS (
    SELECT srcip, time - LAG(time) OVER (PARTITION BY srcip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, AVG(gap) AS avg_interval
FROM gaps GROUP BY srcip HAVING COUNT(*) > 10 ORDER BY avg_interval DESC;

-- 52. SI-DUR — Flow duration (last - first packet time) per source IP.
SELECT srcip, MAX(time) - MIN(time) AS duration
FROM packets GROUP BY srcip ORDER BY duration DESC;

-- 53. SI-BRATE — Byte rate (bytes / duration) per source IP (> 1 packet; zero duration -> 1).
SELECT srcip,
       SUM(pkt_len) / CASE WHEN MAX(time) - MIN(time) = 0 THEN 1
                           ELSE MAX(time) - MIN(time) END AS byte_rate
FROM packets GROUP BY srcip HAVING COUNT(*) > 1 ORDER BY byte_rate DESC;

-- 54. SI-SINT — Standard deviation of inter-arrival times per source IP.
WITH gaps AS (
    SELECT srcip, time - LAG(time) OVER (PARTITION BY srcip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, STDDEV_POP(gap) AS std_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY srcip;

-- 55. SI-CINT — Coefficient of variation (std / mean) of inter-arrival times per source IP.
WITH gaps AS (
    SELECT srcip, time - LAG(time) OVER (PARTITION BY srcip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, STDDEV_POP(gap) / AVG(gap) AS cv_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY srcip HAVING AVG(gap) > 0;

-- 3b. Per destination IP

-- 56. DI-AINT — Average packet inter-arrival time per destination IP (> 10 packets).
WITH gaps AS (
    SELECT dstip, time - LAG(time) OVER (PARTITION BY dstip ORDER BY time) AS gap
    FROM packets
)
SELECT dstip, AVG(gap) AS avg_interval
FROM gaps GROUP BY dstip HAVING COUNT(*) > 10 ORDER BY avg_interval DESC;

-- 57. DI-DUR — Flow duration per destination IP.
SELECT dstip, MAX(time) - MIN(time) AS duration
FROM packets GROUP BY dstip ORDER BY duration DESC;

-- 58. DI-BRATE — Byte rate per destination IP (> 1 packet; zero duration -> 1).
SELECT dstip,
       SUM(pkt_len) / CASE WHEN MAX(time) - MIN(time) = 0 THEN 1
                           ELSE MAX(time) - MIN(time) END AS byte_rate
FROM packets GROUP BY dstip HAVING COUNT(*) > 1 ORDER BY byte_rate DESC;

-- 59. DI-SINT — Standard deviation of inter-arrival times per destination IP.
WITH gaps AS (
    SELECT dstip, time - LAG(time) OVER (PARTITION BY dstip ORDER BY time) AS gap
    FROM packets
)
SELECT dstip, STDDEV_POP(gap) AS std_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY dstip;

-- 60. DI-CINT — Coefficient of variation of inter-arrival times per destination IP.
WITH gaps AS (
    SELECT dstip, time - LAG(time) OVER (PARTITION BY dstip ORDER BY time) AS gap
    FROM packets
)
SELECT dstip, STDDEV_POP(gap) / AVG(gap) AS cv_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY dstip HAVING AVG(gap) > 0;

-- 3c. Per IP pair (srcip, dstip)

-- 61. PR-AINT — Average packet inter-arrival time per IP pair (> 10 packets).
WITH gaps AS (
    SELECT srcip, dstip,
           time - LAG(time) OVER (PARTITION BY srcip, dstip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, AVG(gap) AS avg_interval
FROM gaps GROUP BY srcip, dstip HAVING COUNT(*) > 10 ORDER BY avg_interval DESC;

-- 62. PR-DUR — Flow duration per IP pair.
SELECT srcip, dstip, MAX(time) - MIN(time) AS duration
FROM packets GROUP BY srcip, dstip ORDER BY duration DESC;

-- 63. PR-BRATE — Byte rate per IP pair (> 1 packet; zero duration -> 1).
SELECT srcip, dstip,
       SUM(pkt_len) / CASE WHEN MAX(time) - MIN(time) = 0 THEN 1
                           ELSE MAX(time) - MIN(time) END AS byte_rate
FROM packets GROUP BY srcip, dstip HAVING COUNT(*) > 1 ORDER BY byte_rate DESC;

-- 64. PR-SINT — Standard deviation of inter-arrival times per IP pair.
WITH gaps AS (
    SELECT srcip, dstip,
           time - LAG(time) OVER (PARTITION BY srcip, dstip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, STDDEV_POP(gap) AS std_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY srcip, dstip;

-- 65. PR-CINT — Coefficient of variation of inter-arrival times per IP pair.
WITH gaps AS (
    SELECT srcip, dstip,
           time - LAG(time) OVER (PARTITION BY srcip, dstip ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, STDDEV_POP(gap) / AVG(gap) AS cv_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY srcip, dstip HAVING AVG(gap) > 0;

-- 3d. Per 5-tuple flow (srcip, dstip, srcport, dstport, proto)

-- 66. FT-AINT — Average packet inter-arrival time per 5-tuple flow (> 10 packets).
WITH gaps AS (
    SELECT srcip, dstip, srcport, dstport, proto,
           time - LAG(time) OVER (
               PARTITION BY srcip, dstip, srcport, dstport, proto ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, srcport, dstport, proto, AVG(gap) AS avg_interval
FROM gaps GROUP BY srcip, dstip, srcport, dstport, proto
HAVING COUNT(*) > 10 ORDER BY avg_interval DESC;

-- 67. FT-DUR — Flow duration per 5-tuple flow.
SELECT srcip, dstip, srcport, dstport, proto, MAX(time) - MIN(time) AS duration
FROM packets GROUP BY srcip, dstip, srcport, dstport, proto ORDER BY duration DESC;

-- 68. FT-BRATE — Byte rate per 5-tuple flow (> 1 packet; zero duration -> 1).
SELECT srcip, dstip, srcport, dstport, proto,
       SUM(pkt_len) / CASE WHEN MAX(time) - MIN(time) = 0 THEN 1
                           ELSE MAX(time) - MIN(time) END AS byte_rate
FROM packets GROUP BY srcip, dstip, srcport, dstport, proto
HAVING COUNT(*) > 1 ORDER BY byte_rate DESC;

-- 69. FT-SINT — Standard deviation of inter-arrival times per 5-tuple flow.
WITH gaps AS (
    SELECT srcip, dstip, srcport, dstport, proto,
           time - LAG(time) OVER (
               PARTITION BY srcip, dstip, srcport, dstport, proto ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, srcport, dstport, proto, STDDEV_POP(gap) AS std_iat
FROM gaps WHERE gap IS NOT NULL GROUP BY srcip, dstip, srcport, dstport, proto;

-- 70. FT-CINT — Coefficient of variation of inter-arrival times per 5-tuple flow.
WITH gaps AS (
    SELECT srcip, dstip, srcport, dstport, proto,
           time - LAG(time) OVER (
               PARTITION BY srcip, dstip, srcport, dstport, proto ORDER BY time) AS gap
    FROM packets
)
SELECT srcip, dstip, srcport, dstport, proto, STDDEV_POP(gap) / AVG(gap) AS cv_iat
FROM gaps WHERE gap IS NOT NULL
GROUP BY srcip, dstip, srcport, dstport, proto HAVING AVG(gap) > 0;
