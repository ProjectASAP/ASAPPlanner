-- sidra's `workload/tpch_gate_slice_rich.yaml`: 77 data-quality checks over TPC-H, reading
-- `lineitem`. One SQL statement per check.
--
-- Each check is fused into one query by sidra's `emit_sql_set.fused`: every
-- metric node becomes a CTE and the assertion node becomes the outer SELECT, so
-- its threshold travels with the query. Calls to sidra's own functions
-- (`entropy`, `cramers_v`, `tvd_similarity`, ...; `workload/udf/`) are left
-- as written: they are not DataFusion built-ins.
--
-- Regenerate with, from the sidra repo:
--   uv run python -c "import sys; sys.path.insert(0, 'pyscripts/asap-planner_test'); from emit_sql_set import fused; [print(f'-- {i}\n{s}\n') for i, _, s in fused('workload/tpch_gate_slice_rich.yaml', ['lineitem'])[0]]"
--
-- ONE QUERY PER LINE: `-- <test id>` then the query. The reader splits on
-- newlines, not on `;`.

-- UG-Completeness-001
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-002
WITH metric AS (SELECT COUNT(l_suppkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-003
WITH metric AS (SELECT COUNT(l_extendedprice) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-004
WITH metric AS (SELECT COUNT(l_partkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-005
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-006
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-007
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-008
WITH metric AS (SELECT COUNT(l_discount) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-009
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Uniqueness-001
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS distinctness FROM lineitem) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- UG-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_linenumber BETWEEN 1 AND 7 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('N', 'R', 'A') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN l_tax BETWEEN 0 AND 0.08 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0 AND 0.1 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice >= 900 AND l_extendedprice <= 105000 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Distribution-001
WITH metric AS (SELECT MIN(l_quantity) AS m FROM lineitem) SELECT m, m >= 1 AS ok FROM metric

-- UG-Distribution-002
WITH metric AS (SELECT AVG(l_tax) AS m FROM lineitem) SELECT m, m BETWEEN 0.039 AND 0.041 AS ok FROM metric

-- UG-Distribution-003
WITH metric AS (SELECT MAX(l_discount) AS m FROM lineitem) SELECT m, m <= 0.1 AS ok FROM metric

-- UG-Distribution-004
WITH metric AS (SELECT MIN(l_extendedprice) AS m FROM lineitem) SELECT m, m >= 900 AS ok FROM metric

-- UG-Distribution-005
WITH metric AS (SELECT MAX(l_quantity) AS m FROM lineitem) SELECT m, m <= 50 AS ok FROM metric

-- UG-Distribution-006
WITH metric AS (SELECT STDDEV_POP(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 22500 AND 24100 AS ok FROM metric

-- UG-Distribution-007
WITH metric AS (SELECT AVG(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 25.2 AND 25.8 AS ok FROM metric

-- UG-Distribution-008
WITH metric AS (SELECT STDDEV_POP(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 14.1 AND 14.8 AS ok FROM metric

-- UG-Distribution-009
WITH metric AS (SELECT MIN(l_tax) AS m FROM lineitem) SELECT m, m >= 0 AS ok FROM metric

-- UG-Distribution-010
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 37500 AND 39000 AS ok FROM metric

-- UG-Distribution-011
WITH metric AS (SELECT AVG(l_discount) AS m FROM lineitem) SELECT m, m BETWEEN 0.049 AND 0.051 AS ok FROM metric

-- UG-Distribution-012
WITH metric AS (SELECT MIN(l_discount) AS m FROM lineitem) SELECT m, m >= 0 AS ok FROM metric

-- UG-Distribution-013
WITH metric AS (SELECT MAX(l_extendedprice) AS m FROM lineitem) SELECT m, m <= 105000 AS ok FROM metric

-- UG-Distribution-014
WITH metric AS (SELECT MAX(l_tax) AS m FROM lineitem) SELECT m, m <= 0.08 AS ok FROM metric

-- UG-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem) SELECT n, n >= 19000000 AS ok FROM metric

-- UG-Freshness-001
WITH metric AS (SELECT MAX(l_receiptdate) AS latest FROM lineitem) SELECT latest, latest >= CAST('1998-12-01' AS DATE) AS ok FROM metric

-- UG-Freshness-002
WITH metric AS (SELECT MAX(l_shipdate) AS latest FROM lineitem) SELECT latest, latest >= CAST('1998-11-01' AS DATE) AS ok FROM metric

-- US-Completeness-001
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'N') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-002
WITH metric AS (SELECT COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_shipmode = 'AIR') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-003
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'F') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-004
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'F') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-005
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'F') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-006
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'R') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-007
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'R') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Completeness-008
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'N') SELECT completeness, completeness >= 1 AS ok FROM metric

-- US-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'N') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linestatus = 'F') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'R') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'N') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'N') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'R') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-010
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'COLLECT COD') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-011
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'COLLECT COD') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-012
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-013
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'AIR') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-014
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'COLLECT COD') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-015
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-016
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-017
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' OR l_linestatus = 'F' THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-018
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' OR l_linestatus = 'F' THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-019
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-020
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate < l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'AIR') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Distribution-001
WITH metric AS (SELECT MIN(l_extendedprice) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m >= 800 AS ok FROM metric

-- US-Distribution-002
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m BETWEEN 36500 AND 40000 AS ok FROM metric

-- US-Distribution-003
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT m, m BETWEEN 36500 AND 40000 AS ok FROM metric

-- US-Distribution-004
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem WHERE l_linenumber = 1) SELECT m, m BETWEEN 36500 AND 40000 AS ok FROM metric

-- US-Distribution-005
WITH metric AS (SELECT AVG(l_quantity) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m BETWEEN 24.5 AND 26.5 AS ok FROM metric

-- US-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-002
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_linenumber = 1) SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-003
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT n, n >= 2570000 AS ok FROM metric

-- US-Volume-004
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipinstruct = 'COLLECT COD') SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-005
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipmode = 'AIR') SELECT n, n >= 2570000 AS ok FROM metric

-- US-Volume-006
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_returnflag = 'R') SELECT n, n >= 4450000 AS ok FROM metric

-- US-Volume-007
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_linestatus = 'F') SELECT n, n >= 9000000 AS ok FROM metric

-- US-Volume-008
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_returnflag = 'N') SELECT n, n >= 9100000 AS ok FROM metric
