-- sidra's `workload/tpch_gate_complex_rich.yaml`: 124 data-quality checks over TPC-H, reading
-- `lineitem`. One SQL statement per check.
--
-- Each check is fused into one query by sidra's `emit_sql_set.fused`: every
-- metric node becomes a CTE and the assertion node becomes the outer SELECT, so
-- its threshold travels with the query. Calls to sidra's own functions
-- (`entropy`, `cramers_v`, `tvd_similarity`, ...; `workload/udf/`) are left
-- as written: they are not DataFusion built-ins.
--
-- Regenerate with, from the sidra repo:
--   uv run python -c "import sys; sys.path.insert(0, 'pyscripts/asap-planner_test'); from emit_sql_set import fused; [print(f'-- {i}\n{s}\n') for i, _, s in fused('workload/tpch_gate_complex_rich.yaml', ['lineitem'])[0]]"
--
-- ONE QUERY PER LINE: `-- <test id>` then the query. The reader splits on
-- newlines, not on `;`.

-- UG-Completeness-001
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-002
WITH metric AS (SELECT COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-003
WITH metric AS (SELECT COUNT(l_suppkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-004
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-005
WITH metric AS (SELECT COUNT(l_tax) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-006
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-007
WITH metric AS (SELECT COUNT(l_extendedprice) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-008
WITH metric AS (SELECT COUNT(l_partkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-009
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-010
WITH metric AS (SELECT COUNT(l_linenumber) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-011
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-012
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-013
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-014
WITH metric AS (SELECT COUNT(l_discount) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-015
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Uniqueness-001
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS distinctness FROM lineitem) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- UG-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_linestatus IN ('F', 'O') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN l_linenumber BETWEEN 1 AND 7 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN l_tax BETWEEN 0.0 AND 0.08 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > l_quantity * 900 AND l_extendedprice <= l_quantity * 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0.0 AND 0.1 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('N', 'R', 'A') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-010
WITH metric AS (SELECT AVG(CASE WHEN l_suppkey BETWEEN 1 AND 170000 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-011
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-012
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate BETWEEN CAST('1992-01-01' AS DATE) AND CAST('1998-12-31' AS DATE) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

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
WITH metric AS (SELECT AVG(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 25.3 AND 25.7 AS ok FROM metric

-- UG-Distribution-007
WITH metric AS (SELECT MIN(l_tax) AS m FROM lineitem) SELECT m, m >= 0 AS ok FROM metric

-- UG-Distribution-008
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 37500 AND 39000 AS ok FROM metric

-- UG-Distribution-009
WITH metric AS (SELECT AVG(l_discount) AS m FROM lineitem) SELECT m, m BETWEEN 0.049 AND 0.051 AS ok FROM metric

-- UG-Distribution-010
WITH metric AS (SELECT MIN(l_discount) AS m FROM lineitem) SELECT m, m >= 0 AS ok FROM metric

-- UG-Distribution-011
WITH metric AS (SELECT MAX(l_extendedprice) AS m FROM lineitem) SELECT m, m <= 105000 AS ok FROM metric

-- UG-Distribution-012
WITH metric AS (SELECT MAX(l_tax) AS m FROM lineitem) SELECT m, m <= 0.08 AS ok FROM metric

-- UG-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem) SELECT n, n >= 19000000 AS ok FROM metric

-- UG-Freshness-001
WITH metric AS (SELECT MAX(l_receiptdate) AS latest FROM lineitem) SELECT latest, latest >= CAST('1998-12-01' AS DATE) AS ok FROM metric

-- UG-Freshness-002
WITH metric AS (SELECT MAX(l_shipdate) AS latest FROM lineitem) SELECT latest, latest >= CAST('1998-11-01' AS DATE) AS ok FROM metric

-- UG-Distribution-013
WITH metric AS (SELECT MODE_FREQUENCY(l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.49 AND 0.52 AS ok FROM metric

-- UG-Distribution-014
WITH metric AS (SELECT GINI_IMPURITY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 0.74 AND 0.76 AS ok FROM metric

-- UG-Distribution-015
WITH metric AS (SELECT MODE_FREQUENCY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0.138 AND 0.148 AS ok FROM metric

-- UG-Distribution-016
WITH metric AS (SELECT SKEWNESS(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- UG-Distribution-017
WITH metric AS (SELECT IQR(l_discount) AS m FROM lineitem) SELECT m, m BETWEEN 0.05 AND 0.07 AS ok FROM metric

-- UG-Distribution-018
WITH metric AS (SELECT IQR(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 23 AND 27 AS ok FROM metric

-- UG-Distribution-019
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 1.93 AND 1.946 AS ok FROM metric

-- UG-Distribution-020
WITH metric AS (SELECT IQR(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 35000 AND 38000 AS ok FROM metric

-- UG-Distribution-021
WITH metric AS (SELECT IQR(l_tax) AS m FROM lineitem) SELECT m, m BETWEEN 0.03 AND 0.05 AS ok FROM metric

-- UG-Distribution-022
WITH metric AS (SELECT GINI_IMPURITY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 0.59 AND 0.65 AS ok FROM metric

-- UG-Distribution-023
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- UG-Distribution-024
WITH metric AS (SELECT PERCENTILE_CONT(l_extendedprice, 0.99) AS m FROM lineitem) SELECT m, m BETWEEN 88000 AND 95000 AS ok FROM metric

-- UG-Distribution-025
WITH metric AS (SELECT PERCENTILE_CONT(l_discount, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 0.04 AND 0.06 AS ok FROM metric

-- UG-Distribution-026
WITH metric AS (SELECT KURTOSIS(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN -1.25 AND -1.15 AS ok FROM metric

-- UG-Distribution-027
WITH metric AS (SELECT PERCENTILE_CONT(l_quantity, 0.95) AS m FROM lineitem) SELECT m, m BETWEEN 46 AND 50 AS ok FROM metric

-- UG-Distribution-028
WITH metric AS (SELECT PERCENTILE_CONT(l_tax, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 0.03 AND 0.05 AS ok FROM metric

-- UG-Distribution-029
WITH metric AS (SELECT ENTROPY(l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.68 AND 0.6932 AS ok FROM metric

-- UG-Distribution-030
WITH metric AS (SELECT PERCENTILE_CONT(l_extendedprice, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 35000 AND 38500 AS ok FROM metric

-- UG-Distribution-031
WITH metric AS (SELECT GINI_IMPURITY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0.85 AND 0.8572 AS ok FROM metric

-- UG-Distribution-032
WITH metric AS (SELECT ENTROPY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 1 AND 1.07 AS ok FROM metric

-- UG-Distribution-033
WITH metric AS (SELECT TOP_K_SHARE(l_shipmode, 5) AS m FROM lineitem) SELECT m, m BETWEEN 0.69 AND 0.74 AS ok FROM metric

-- UG-Distribution-034
WITH metric AS (SELECT MODE_FREQUENCY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 0.24 AND 0.26 AS ok FROM metric

-- UG-Distribution-035
WITH metric AS (SELECT PERCENTILE_CONT(l_quantity, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 24 AND 26 AS ok FROM metric

-- UG-Distribution-036
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 0.49 AND 0.52 AS ok FROM metric

-- UG-Distribution-037
WITH metric AS (SELECT SKEWNESS(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 0.29 AND 0.37 AS ok FROM metric

-- UG-Distribution-038
WITH metric AS (SELECT CORR(l_quantity, l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 0.91 AND 0.95 AS ok FROM metric

-- UG-Distribution-039
WITH metric AS (SELECT CORR(l_discount, l_tax) AS m FROM lineitem) SELECT m, m BETWEEN -0.01 AND 0.01 AS ok FROM metric

-- UG-Distribution-040
WITH metric AS (SELECT SPEARMAN(l_quantity, l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 0.92 AND 0.96 AS ok FROM metric

-- UG-Distribution-041
WITH metric AS (SELECT MUTUAL_INFORMATION(l_returnflag, l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.62 AND 0.69 AS ok FROM metric

-- UG-Distribution-042
WITH metric AS (SELECT CRAMERS_V(l_shipinstruct, l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0 AND 0.01 AS ok FROM metric

-- UG-Distribution-043
WITH metric AS (SELECT CORR(l_quantity, l_discount) AS m FROM lineitem) SELECT m, m BETWEEN -0.01 AND 0.01 AS ok FROM metric

-- UG-Distribution-044
WITH metric AS (SELECT COVAR_POP(l_quantity, l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 300000 AND 324000 AS ok FROM metric

-- UG-Distribution-045
WITH metric AS (SELECT CRAMERS_V(l_returnflag, l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.96 AND 1 AS ok FROM metric

-- US-Completeness-001
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'O') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-002
WITH metric AS (SELECT COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_shipmode = 'AIR') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-003
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-004
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'O') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-005
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_linestatus = 'O') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-006
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-007
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linestatus = 'O') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > 0 AND l_extendedprice / l_quantity BETWEEN 900 AND 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' OR l_receiptdate <= CAST('1995-06-17' AS DATE) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linestatus = 'O') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'R') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' OR l_receiptdate <= CAST('1995-06-17' AS DATE) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linestatus = 'O') SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'AIR') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-010
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-011
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-012
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag = 'N' OR l_receiptdate <= CAST('1995-06-17' AS DATE) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Validity-013
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('R', 'A', 'N') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linestatus = 'O') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-014
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-015
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'AIR') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-016
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'N') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-017
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-018
WITH metric AS (SELECT AVG(CASE WHEN l_linestatus IN ('F', 'O') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'N') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-019
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-020
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > 0 AND l_extendedprice / l_quantity BETWEEN 900 AND 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_linenumber = 1) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Validity-021
WITH metric AS (SELECT AVG(CASE WHEN l_linestatus IN ('F', 'O') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_returnflag = 'R') SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- US-Validity-022
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > 0 AND l_extendedprice / l_quantity BETWEEN 900 AND 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- US-Distribution-001
WITH metric AS (SELECT MAX(l_extendedprice) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m <= 105000 AS ok FROM metric

-- US-Distribution-002
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m BETWEEN 37500 AND 38950 AS ok FROM metric

-- US-Distribution-003
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM lineitem WHERE l_linenumber = 1) SELECT m, m BETWEEN 37450 AND 38950 AS ok FROM metric

-- US-Distribution-004
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT m, m BETWEEN 0.49 AND 0.52 AS ok FROM metric

-- US-Distribution-005
WITH metric AS (SELECT ENTROPY(l_linestatus) AS m FROM lineitem WHERE l_returnflag = 'R') SELECT m, m BETWEEN -0.001 AND 0.01 AS ok FROM metric

-- US-Distribution-006
WITH metric AS (SELECT ENTROPY(l_linestatus) AS m FROM lineitem WHERE l_returnflag = 'N') SELECT m, m BETWEEN 0.05 AND 0.09 AS ok FROM metric

-- US-Distribution-007
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM lineitem WHERE l_shipmode = 'AIR') SELECT m, m BETWEEN 1.37 AND 1.395 AS ok FROM metric

-- US-Distribution-008
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM lineitem WHERE l_linestatus = 'O') SELECT m, m BETWEEN 0.999 AND 1 AS ok FROM metric

-- US-Distribution-009
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM lineitem WHERE l_linenumber = 1) SELECT m, m BETWEEN 1.37 AND 1.395 AS ok FROM metric

-- US-Distribution-010
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT m, m BETWEEN 1.93 AND 1.9465 AS ok FROM metric

-- US-Distribution-011
WITH metric AS (SELECT MODE_FREQUENCY(l_shipmode) AS m FROM lineitem WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT m, m BETWEEN 0.1415 AND 0.147 AS ok FROM metric

-- US-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipinstruct = 'TAKE BACK RETURN') SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-002
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_linenumber = 1) SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-003
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_linestatus = 'O') SELECT n, n >= 9000000 AS ok FROM metric

-- US-Volume-004
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipmode = 'TRUCK') SELECT n, n >= 2570000 AS ok FROM metric

-- US-Volume-005
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipmode = 'AIR') SELECT n, n >= 2570000 AS ok FROM metric

-- US-Volume-006
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_returnflag = 'R') SELECT n, n >= 4450000 AS ok FROM metric

-- US-Volume-007
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT n, n >= 4500000 AS ok FROM metric

-- US-Volume-008
WITH metric AS (SELECT COUNT(*) AS n FROM lineitem WHERE l_returnflag = 'N') SELECT n, n >= 9100000 AS ok FROM metric
