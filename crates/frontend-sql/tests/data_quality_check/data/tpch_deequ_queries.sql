-- The DQC warehouse-ingestion check set, as one SQL statement per check.
--
-- Source: sidra's `workload/tpch_deequ.yaml`, 50 checks over TPC-H `lineitem`,
-- each fused into one query by `pyutils.workload.backend.fuse` — every metric
-- node becomes a CTE and the assertion node becomes the outer SELECT, so the
-- threshold a check asserts (`completeness >= 0.99`) travels with the query
-- rather than being stripped off it.
--
-- Regenerate with, from the sidra repo:
--   uv run python pyscripts/asap-planner_test/emit_sql_set.py workload/tpch_deequ.yaml
--
-- ONE QUERY PER LINE, and the reader splits on newlines rather than on `;`:
-- U-P3p's regex literal is '^[a-zA-Z ,.:;!?-]+$', which contains a semicolon,
-- so a `;` split would fragment it. `fuse` emits single-line SQL, so the line
-- is an exact statement boundary and no escaping is needed.

-- U-P1a
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.99 AS ok FROM metric

-- U-P1b
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS orderkey, COUNT(l_partkey) * 1.0 / COUNT(*) AS partkey, COUNT(l_suppkey) * 1.0 / COUNT(*) AS suppkey, COUNT(l_linenumber) * 1.0 / COUNT(*) AS linenumber FROM lineitem) SELECT orderkey, partkey, suppkey, linenumber, orderkey = 1.0 AND partkey = 1.0 AND suppkey = 1.0 AND linenumber = 1.0 AS ok FROM metric

-- U-P1c
WITH metric AS (SELECT COUNT(l_comment) * 1.0 / COUNT(*) AS comment, COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS shipinstruct, COUNT(l_shipmode) * 1.0 / COUNT(*) AS shipmode FROM lineitem) SELECT comment, shipinstruct, shipmode, comment = 1.0 AND shipinstruct = 1.0 AND shipmode = 1.0 AS ok FROM metric

-- U-P1d
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS commitdate, COUNT(l_receiptdate) * 1.0 / COUNT(*) AS receiptdate FROM lineitem) SELECT commitdate, receiptdate, commitdate = 1.0 AND receiptdate = 1.0 AS ok FROM metric

-- U-P1e
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS quantity, COUNT(l_extendedprice) * 1.0 / COUNT(*) AS extendedprice, COUNT(l_discount) * 1.0 / COUNT(*) AS discount, COUNT(l_tax) * 1.0 / COUNT(*) AS tax, COUNT(l_returnflag) * 1.0 / COUNT(*) AS returnflag, COUNT(l_linestatus) * 1.0 / COUNT(*) AS linestatus FROM lineitem) SELECT quantity, extendedprice, discount, tax, returnflag, linestatus, quantity = 1.0 AND extendedprice = 1.0 AND discount = 1.0 AND tax = 1.0 AND returnflag = 1.0 AND linestatus = 1.0 AS ok FROM metric

-- U-P2a
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey) * 1.0 / COUNT(*) AS distinctness FROM lineitem) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- U-P2b
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS pk_distinctness FROM lineitem) SELECT pk_distinctness, pk_distinctness = 1.0 AS ok FROM metric

-- U-P2c
WITH metric AS (SELECT APPROX_DISTINCT(l_partkey) AS parts, APPROX_DISTINCT(l_suppkey) AS suppliers FROM lineitem) SELECT parts, suppliers, parts >= 1000 AND suppliers >= 100 AS ok FROM metric

-- U-P2d
WITH metric AS (SELECT COUNT(DISTINCT l_returnflag) AS nd_returnflag, COUNT(DISTINCT l_linestatus) AS nd_linestatus, COUNT(DISTINCT l_shipmode) AS nd_shipmode, COUNT(DISTINCT l_shipinstruct) AS nd_shipinstruct, COUNT(DISTINCT l_linenumber) AS nd_linenumber, COUNT(DISTINCT l_quantity) AS nd_quantity FROM lineitem) SELECT nd_returnflag, nd_linestatus, nd_shipmode, nd_shipinstruct, nd_linenumber, nd_quantity, nd_returnflag = 3 AND nd_linestatus = 2 AND nd_shipmode = 7 AND nd_shipinstruct = 4 AND nd_linenumber = 7 AND nd_quantity = 50 AS ok FROM metric

-- U-P2e
WITH metric AS (SELECT APPROX_DISTINCT(l_shipdate) AS nd_shipdate, APPROX_DISTINCT(l_commitdate) AS nd_commitdate FROM lineitem) SELECT nd_shipdate, nd_commitdate, nd_shipdate BETWEEN 2300 AND 2800 AND nd_commitdate BETWEEN 2250 AND 2750 AS ok FROM metric

-- U-P3a
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS in_range FROM lineitem) SELECT in_range, in_range = 1.0 AS ok FROM metric

-- U-P3b
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0.00 AND 0.10 THEN 1.0 ELSE 0.0 END) AS discount_ok, AVG(CASE WHEN l_tax BETWEEN 0.00 AND 0.08 THEN 1.0 ELSE 0.0 END) AS tax_ok FROM lineitem) SELECT discount_ok, tax_ok, discount_ok = 1.0 AND tax_ok = 1.0 AS ok FROM metric

-- U-P3c
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('A', 'N', 'R') THEN 1.0 ELSE 0.0 END) AS returnflag_ok, AVG(CASE WHEN l_linestatus IN ('O', 'F') THEN 1.0 ELSE 0.0 END) AS linestatus_ok FROM lineitem) SELECT returnflag_ok, linestatus_ok, returnflag_ok = 1.0 AND linestatus_ok = 1.0 AS ok FROM metric

-- U-P3d
WITH metric AS (SELECT AVG(CASE WHEN REGEXP_LIKE(l_shipmode, '^(AIR|FOB|MAIL|RAIL|REG AIR|SHIP|TRUCK)$') THEN 1.0 ELSE 0.0 END) AS shipmode_ok, AVG(CASE WHEN REGEXP_LIKE(l_shipinstruct, '^(COLLECT COD|DELIVER IN PERSON|NONE|TAKE BACK RETURN)$') THEN 1.0 ELSE 0.0 END) AS shipinstruct_ok FROM lineitem) SELECT shipmode_ok, shipinstruct_ok, shipmode_ok = 1.0 AND shipinstruct_ok = 1.0 AS ok FROM metric

-- U-P3e
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > 0.0 THEN 1.0 ELSE 0.0 END) AS price_positive, AVG(CASE WHEN l_linenumber BETWEEN 1 AND 7 THEN 1.0 ELSE 0.0 END) AS linenumber_ok FROM lineitem) SELECT price_positive, linenumber_ok, price_positive = 1.0 AND linenumber_ok = 1.0 AS ok FROM metric

-- U-P3f
WITH metric AS (SELECT MIN(LENGTH(l_comment)) AS comment_min, MAX(LENGTH(l_comment)) AS comment_max, MAX(LENGTH(l_shipmode)) AS shipmode_max FROM lineitem) SELECT comment_min, comment_max, shipmode_max, comment_min >= 1 AND comment_max <= 44 AND shipmode_max <= 10 AS ok FROM metric

-- U-P3g
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate BETWEEN CAST('1992-01-01' AS DATE) AND CAST('1998-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS in_window, AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS before_receipt FROM lineitem) SELECT in_window, before_receipt, in_window = 1.0 AND before_receipt = 1.0 AS ok FROM metric

-- U-P3h
WITH metric AS (SELECT MIN(l_orderkey) AS min_orderkey, MIN(l_partkey) AS min_partkey, MIN(l_suppkey) AS min_suppkey FROM lineitem) SELECT min_orderkey, min_partkey, min_suppkey, min_orderkey >= 1 AND min_partkey >= 1 AND min_suppkey >= 1 AS ok FROM metric

-- U-P3i
WITH metric AS (SELECT AVG(CASE WHEN l_orderkey > 0 AND l_orderkey % 32 < 8 THEN 1.0 ELSE 0.0 END) AS on_grid FROM lineitem) SELECT on_grid, on_grid = 1.0 AS ok FROM metric

-- U-P3j
WITH metric AS (SELECT AVG(CASE WHEN l_discount IN (0.00, 0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.10) THEN 1.0 ELSE 0.0 END) AS discount_on_grid, AVG(CASE WHEN l_tax IN (0.00, 0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08) THEN 1.0 ELSE 0.0 END) AS tax_on_grid FROM lineitem) SELECT discount_on_grid, tax_on_grid, discount_on_grid = 1.0 AND tax_on_grid = 1.0 AS ok FROM metric

-- U-P3k
WITH metric AS (SELECT AVG(CASE WHEN l_quantity = FLOOR(l_quantity) THEN 1.0 ELSE 0.0 END) AS integral FROM lineitem) SELECT integral, integral = 1.0 AS ok FROM metric

-- U-P3l
WITH metric AS (SELECT AVG(CASE WHEN l_commitdate BETWEEN CAST('1992-01-31' AS DATE) AND CAST('1998-10-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS commit_window, AVG(CASE WHEN l_receiptdate BETWEEN CAST('1992-01-03' AS DATE) AND CAST('1998-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS receipt_window FROM lineitem) SELECT commit_window, receipt_window, commit_window = 1.0 AND receipt_window = 1.0 AS ok FROM metric

-- U-P3m
WITH metric AS (SELECT AVG(CASE WHEN l_receiptdate > l_shipdate THEN 1.0 ELSE 0.0 END) AS lag_at_least_1, AVG(CASE WHEN l_receiptdate <= l_shipdate + INTERVAL '30' DAY THEN 1.0 ELSE 0.0 END) AS lag_at_most_30 FROM lineitem) SELECT lag_at_least_1, lag_at_most_30, lag_at_least_1 = 1.0 AND lag_at_most_30 = 1.0 AS ok FROM metric

-- U-P3n
WITH metric AS (SELECT AVG(CASE WHEN (l_shipdate > CAST('1995-06-17' AS DATE)) = (l_linestatus = 'O') THEN 1.0 ELSE 0.0 END) AS linestatus_agrees, AVG(CASE WHEN (l_receiptdate <= CAST('1995-06-17' AS DATE)) = (l_returnflag IN ('R', 'A')) THEN 1.0 ELSE 0.0 END) AS returnflag_agrees FROM lineitem) SELECT linestatus_agrees, returnflag_agrees, linestatus_agrees = 1.0 AND returnflag_agrees = 1.0 AS ok FROM metric

-- U-P3o
WITH metric AS (SELECT MIN(LENGTH(l_returnflag)) AS rf_min, MAX(LENGTH(l_returnflag)) AS rf_max, MIN(LENGTH(l_linestatus)) AS ls_min, MAX(LENGTH(l_linestatus)) AS ls_max, MIN(LENGTH(l_shipinstruct)) AS si_min, MAX(LENGTH(l_shipinstruct)) AS si_max, MIN(LENGTH(l_shipmode)) AS sm_min FROM lineitem) SELECT rf_min, rf_max, ls_min, ls_max, si_min, si_max, sm_min, rf_min = 1 AND rf_max = 1 AND ls_min = 1 AND ls_max = 1 AND si_min = 4 AND si_max = 17 AND sm_min = 3 AS ok FROM metric

-- U-P3p
WITH metric AS (SELECT AVG(CASE WHEN REGEXP_LIKE(l_comment, '^[a-zA-Z ,.:;!?-]+$') THEN 1.0 ELSE 0.0 END) AS comment_charset, AVG(CASE WHEN REGEXP_LIKE(l_shipinstruct, '^[A-Z ]+$') THEN 1.0 ELSE 0.0 END) AS shipinstruct_charset FROM lineitem) SELECT comment_charset, shipinstruct_charset, comment_charset = 1.0 AND shipinstruct_charset = 1.0 AS ok FROM metric

-- U-P3q
WITH metric AS (SELECT AVG(CASE WHEN (l_extendedprice * 100) % l_quantity = 0 THEN 1.0 ELSE 0.0 END) AS recomputable FROM lineitem) SELECT recomputable, recomputable = 1.0 AS ok FROM metric

-- U-P4a
WITH metric AS (SELECT AVG(l_extendedprice) AS mean_price, APPROX_PERCENTILE_CONT(l_discount, 0.99) AS p99_discount FROM lineitem) SELECT mean_price, p99_discount, mean_price BETWEEN 30000 AND 40000 AND p99_discount <= 0.10 AS ok FROM metric

-- U-P4b
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.50) AS p50, APPROX_PERCENTILE_CONT(l_quantity, 0.90) AS p90, APPROX_PERCENTILE_CONT(l_quantity, 0.99) AS p99 FROM lineitem) SELECT p50, p90, p99, p50 BETWEEN 20 AND 31 AND p90 BETWEEN 40 AND 50 AND p99 BETWEEN 45 AND 50 AS ok FROM metric

-- U-P4c
WITH metric AS (SELECT AVG(l_quantity) AS mean_qty, STDDEV_POP(l_quantity) AS sd_qty, MIN(l_quantity) AS min_qty, MAX(l_quantity) AS max_qty FROM lineitem) SELECT mean_qty, sd_qty, min_qty, max_qty, mean_qty BETWEEN 25 AND 26 AND sd_qty BETWEEN 14 AND 15 AND min_qty >= 1 AND max_qty <= 50 AS ok FROM metric

-- U-P4d
WITH metric AS (SELECT CORR(l_quantity, l_extendedprice) AS r FROM lineitem) SELECT r, r BETWEEN 0.80 AND 1.00 AS ok FROM metric

-- U-P4e
WITH metric AS (SELECT SUM(l_quantity) AS total_qty, SUM(l_extendedprice) AS total_price FROM lineitem) SELECT total_qty, total_price, total_qty > 0 AND total_price > 0 AS ok FROM metric

-- U-P4f
WITH metric AS (SELECT AVG(l_discount) AS mean_discount, AVG(l_tax) AS mean_tax, STDDEV_POP(l_discount) AS sd_discount, STDDEV_POP(l_tax) AS sd_tax FROM lineitem) SELECT mean_discount, mean_tax, sd_discount, sd_tax, mean_discount BETWEEN 0.048 AND 0.052 AND mean_tax BETWEEN 0.038 AND 0.042 AND sd_discount BETWEEN 0.031 AND 0.032 AND sd_tax BETWEEN 0.025 AND 0.026 AS ok FROM metric

-- U-P4g
WITH metric AS (SELECT AVG(l_linenumber) AS mean_lineno, MIN(l_linenumber) AS min_lineno, MAX(l_linenumber) AS max_lineno FROM lineitem) SELECT mean_lineno, min_lineno, max_lineno, mean_lineno BETWEEN 2.9 AND 3.1 AND min_lineno >= 1 AND max_lineno <= 7 AS ok FROM metric

-- U-P4h
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.50) AS p50_price, APPROX_PERCENTILE_CONT(l_extendedprice, 0.95) AS p95_price FROM lineitem) SELECT p50_price, p95_price, p50_price BETWEEN 30000 AND 40000 AND p95_price BETWEEN 65000 AND 85000 AS ok FROM metric

-- U-P4i
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' THEN 1.0 ELSE 0.0 END) AS frac_new, AVG(CASE WHEN l_linestatus = 'O' THEN 1.0 ELSE 0.0 END) AS frac_open, AVG(CASE WHEN l_shipmode = 'AIR' THEN 1.0 ELSE 0.0 END) AS frac_air, AVG(CASE WHEN l_shipinstruct = 'NONE' THEN 1.0 ELSE 0.0 END) AS frac_no_instruction FROM lineitem) SELECT frac_new, frac_open, frac_air, frac_no_instruction, frac_new BETWEEN 0.45 AND 0.55 AND frac_open BETWEEN 0.45 AND 0.55 AND frac_air BETWEEN 0.13 AND 0.16 AND frac_no_instruction BETWEEN 0.23 AND 0.27 AS ok FROM metric

-- U-P4j
WITH metric AS (SELECT AVG(CASE WHEN l_commitdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS late_rate FROM lineitem) SELECT late_rate, late_rate BETWEEN 0.58 AND 0.68 AS ok FROM metric

-- U-P4k
WITH metric AS (SELECT MIN(l_extendedprice) AS min_price, MAX(l_extendedprice) AS max_price, MIN(l_discount) AS min_discount, MAX(l_discount) AS max_discount, MIN(l_tax) AS min_tax, MAX(l_tax) AS max_tax FROM lineitem) SELECT min_price, max_price, min_discount, max_discount, min_tax, max_tax, min_price >= 900 AND max_price <= 104950 AND min_discount = 0.00 AND max_discount = 0.10 AND min_tax = 0.00 AND max_tax = 0.08 AS ok FROM metric

-- U-P4l
WITH metric AS (SELECT CORR(l_discount, l_tax) AS r_discount_tax, CORR(l_quantity, l_discount) AS r_quantity_discount FROM lineitem) SELECT r_discount_tax, r_quantity_discount, r_discount_tax BETWEEN -0.02 AND 0.02 AND r_quantity_discount BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- U-P4m
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_discount, 0.25) AS d25, APPROX_PERCENTILE_CONT(l_discount, 0.75) AS d75, APPROX_PERCENTILE_CONT(l_tax, 0.25) AS t25, APPROX_PERCENTILE_CONT(l_tax, 0.75) AS t75 FROM lineitem) SELECT d25, d75, t25, t75, d25 BETWEEN 0.01 AND 0.03 AND d75 BETWEEN 0.07 AND 0.09 AND t25 BETWEEN 0.01 AND 0.03 AND t75 BETWEEN 0.05 AND 0.07 AS ok FROM metric

-- U-P4n
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_linenumber, 0.50) AS ln50, APPROX_PERCENTILE_CONT(l_linenumber, 0.90) AS ln90 FROM lineitem) SELECT ln50, ln90, ln50 BETWEEN 2 AND 4 AND ln90 BETWEEN 5 AND 7 AS ok FROM metric

-- U-P4o
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate BETWEEN CAST('1993-01-01' AS DATE) AND CAST('1993-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS y1993, AVG(CASE WHEN l_shipdate BETWEEN CAST('1995-01-01' AS DATE) AND CAST('1995-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS y1995, AVG(CASE WHEN l_shipdate BETWEEN CAST('1997-01-01' AS DATE) AND CAST('1997-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS y1997 FROM lineitem) SELECT y1993, y1995, y1997, y1993 BETWEEN 0.13 AND 0.17 AND y1995 BETWEEN 0.13 AND 0.17 AND y1997 BETWEEN 0.13 AND 0.17 AS ok FROM metric

-- U-P7a
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem) SELECT n, n > 0 AS ok FROM metric

-- U-P8a
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate >= CAST('1998-11-01' AS DATE) THEN 1.0 ELSE 0.0 END) AS fresh FROM lineitem) SELECT fresh, fresh > 0.0 AS ok FROM metric

-- U-P8b
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= CAST('1998-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS not_future FROM lineitem) SELECT not_future, not_future = 1.0 AS ok FROM metric

-- U-P8c
WITH metric AS (SELECT AVG(CASE WHEN l_receiptdate >= CAST('1998-12-01' AS DATE) THEN 1.0 ELSE 0.0 END) AS recent FROM lineitem) SELECT recent, recent > 0.0 AS ok FROM metric

-- U-P8d
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < CAST('1992-02-01' AS DATE) THEN 1.0 ELSE 0.0 END) AS oldest FROM lineitem) SELECT oldest, oldest > 0.0 AS ok FROM metric

-- U-P9a
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice >= l_quantity * 900.0 THEN 1.0 ELSE 0.0 END) AS holds FROM lineitem) SELECT holds, holds = 1.0 AS ok FROM metric

-- U-P9b
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice <= l_quantity * 2099.0 THEN 1.0 ELSE 0.0 END) AS price_upper, AVG(CASE WHEN l_extendedprice * (1 - l_discount) * (1 + l_tax) >= 0.0 THEN 1.0 ELSE 0.0 END) AS charge_nonneg FROM lineitem) SELECT price_upper, charge_nonneg, price_upper = 1.0 AND charge_nonneg = 1.0 AS ok FROM metric

-- U-P9c
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice * (1 - l_discount) * (1 + l_tax) BETWEEN l_extendedprice * 0.90 AND l_extendedprice * 1.08 THEN 1.0 ELSE 0.0 END) AS in_band FROM lineitem) SELECT in_band, in_band = 1.0 AS ok FROM metric
