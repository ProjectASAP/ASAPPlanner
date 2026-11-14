-- sidra's `workload/tpch_ml_basic_rich.yaml`: 70 data-quality checks over TPC-H, reading
-- `online` (data under test) and `training` (reference), both lineitem-shaped. One SQL statement per check.
--
-- Each check is fused into one query by sidra's `emit_sql_set.fused`: every
-- metric node becomes a CTE and the assertion node becomes the outer SELECT, so
-- its threshold travels with the query. Calls to sidra's own functions
-- (`entropy`, `cramers_v`, `tvd_similarity`, ...; `workload/udf/`) are left
-- as written: they are not DataFusion built-ins.
--
-- Regenerate with, from the sidra repo:
--   uv run python -c "import sys; sys.path.insert(0, 'pyscripts/asap-planner_test'); from emit_sql_set import fused; [print(f'-- {i}\n{s}\n') for i, _, s in fused('workload/tpch_ml_basic_rich.yaml', ['online', 'training'])[0]]"
--
-- ONE QUERY PER LINE: `-- <test id>` then the query. The reader splits on
-- newlines, not on `;`.

-- UG-Completeness-001
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-002
WITH metric AS (SELECT COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-003
WITH metric AS (SELECT COUNT(l_suppkey) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-004
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-005
WITH metric AS (SELECT COUNT(l_tax) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-006
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-007
WITH metric AS (SELECT COUNT(l_extendedprice) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-008
WITH metric AS (SELECT COUNT(l_partkey) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-009
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-010
WITH metric AS (SELECT COUNT(l_linenumber) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-011
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-012
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-013
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-014
WITH metric AS (SELECT COUNT(l_discount) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-015
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Uniqueness-001
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS distinctness FROM online) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- UG-Uniqueness-002
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_partkey) * 1.0 / COUNT(*) AS distinctness FROM online) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- UG-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_linestatus IN ('F', 'O') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN l_linenumber BETWEEN 1 AND 7 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('N', 'R', 'A') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_tax BETWEEN 0 AND 0.08 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice >= l_quantity * 900 AND l_extendedprice <= l_quantity * 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0 AND 0.1 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-010
WITH metric AS (SELECT AVG(CASE WHEN l_suppkey BETWEEN 1 AND 170000 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-011
WITH metric AS (SELECT AVG(CASE WHEN l_receiptdate > l_shipdate AND l_receiptdate <= l_shipdate + INTERVAL '30' DAY THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-012
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Distribution-001
WITH metric AS (SELECT MIN(l_quantity) AS m FROM online) SELECT m, m >= 0.9 AS ok FROM metric

-- UG-Distribution-002
WITH metric AS (SELECT AVG(l_tax) AS m FROM online) SELECT m, m BETWEEN 0.038 AND 0.042 AS ok FROM metric

-- UG-Distribution-003
WITH metric AS (SELECT MAX(l_discount) AS m FROM online) SELECT m, m <= 0.110000001 AS ok FROM metric

-- UG-Distribution-004
WITH metric AS (SELECT VARIANCE_POP(l_quantity) AS m FROM online) SELECT m, m BETWEEN 195 AND 222 AS ok FROM metric

-- UG-Distribution-005
WITH metric AS (SELECT MIN(l_extendedprice) AS m FROM online) SELECT m, m >= 810.765 AS ok FROM metric

-- UG-Distribution-006
WITH metric AS (SELECT MAX(l_quantity) AS m FROM online) SELECT m, m <= 55.0 AS ok FROM metric

-- UG-Distribution-007
WITH metric AS (SELECT VARIANCE_POP(l_extendedprice) AS m FROM online) SELECT m, m BETWEEN 500000000 AND 585000000 AS ok FROM metric

-- UG-Distribution-008
WITH metric AS (SELECT AVG(l_quantity) AS m FROM online) SELECT m, m BETWEEN 25.2 AND 25.8 AS ok FROM metric

-- UG-Distribution-009
WITH metric AS (SELECT MIN(l_tax) AS m FROM online) SELECT m, m >= -0.008 AS ok FROM metric

-- UG-Distribution-010
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM online) SELECT m, m BETWEEN 37500 AND 38950 AS ok FROM metric

-- UG-Distribution-011
WITH metric AS (SELECT AVG(l_discount) AS m FROM online) SELECT m, m BETWEEN 0.048 AND 0.052 AS ok FROM metric

-- UG-Distribution-012
WITH metric AS (SELECT MIN(l_discount) AS m FROM online) SELECT m, m >= -0.0100000001 AS ok FROM metric

-- UG-Distribution-013
WITH metric AS (SELECT MAX(l_extendedprice) AS m FROM online) SELECT m, m <= 115436.2 AS ok FROM metric

-- UG-Distribution-014
WITH metric AS (SELECT MAX(l_tax) AS m FROM online) SELECT m, m <= 0.088 AS ok FROM metric

-- UG-Distribution-015
WITH metric AS (SELECT MODE_FREQUENCY(l_linestatus) AS m FROM online) SELECT m, m BETWEEN 0.49 AND 0.51 AS ok FROM metric

-- UG-Distribution-016
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.25) AS m FROM online) SELECT m, m BETWEEN 18500 AND 19500 AS ok FROM metric

-- UG-Distribution-017
WITH metric AS (SELECT GINI_IMPURITY(l_shipinstruct) AS m FROM online) SELECT m, m BETWEEN 0.74 AND 0.7502 AS ok FROM metric

-- UG-Distribution-018
WITH metric AS (SELECT MODE_FREQUENCY(l_shipmode) AS m FROM online) SELECT m, m BETWEEN 0.139 AND 0.148 AS ok FROM metric

-- UG-Distribution-019
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.75) AS m FROM online) SELECT m, m BETWEEN 37 AND 39 AS ok FROM metric

-- UG-Distribution-020
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM online) SELECT m, m BETWEEN 1.7513184 AND 2.14050028 AS ok FROM metric

-- UG-Distribution-021
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.5) AS m FROM online) SELECT m, m BETWEEN 33082.8643 AND 40434.612 AS ok FROM metric

-- UG-Distribution-022
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM online) SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- UG-Distribution-023
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.25) AS m FROM online) SELECT m, m BETWEEN 12 AND 14 AS ok FROM metric

-- UG-Distribution-024
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.95) AS m FROM online) SELECT m, m BETWEEN 46 AND 50 AS ok FROM metric

-- UG-Distribution-025
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.05) AS m FROM online) SELECT m, m BETWEEN 3800 AND 4800 AS ok FROM metric

-- UG-Distribution-026
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.75) AS m FROM online) SELECT m, m BETWEEN 52000 AND 58000 AS ok FROM metric

-- UG-Distribution-027
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_discount, 0.5) AS m FROM online) SELECT m, m BETWEEN 0.04 AND 0.06 AS ok FROM metric

-- UG-Distribution-028
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.95) AS m FROM online) SELECT m, m BETWEEN 76000 AND 83000 AS ok FROM metric

-- UG-Distribution-029
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_tax, 0.5) AS m FROM online) SELECT m, m BETWEEN 0.03 AND 0.05 AS ok FROM metric

-- UG-Distribution-030
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.5) AS m FROM online) SELECT m, m BETWEEN 24 AND 27 AS ok FROM metric

-- UG-Distribution-031
WITH metric AS (SELECT ENTROPY(l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.98 AND 1.09 AS ok FROM metric

-- UG-Distribution-032
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.48 AND 0.53 AS ok FROM metric

-- UG-Distribution-033
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_discount, 0.75) AS m FROM online) SELECT m, m BETWEEN 0.07 AND 0.09 AS ok FROM metric

-- UG-Distribution-034
WITH metric AS (SELECT CRAMERS_V(l_shipinstruct, l_shipmode) AS m FROM online) SELECT m, m BETWEEN 0 AND 0.01 AS ok FROM metric

-- UG-Distribution-035
WITH metric AS (SELECT MUTUAL_INFORMATION(l_returnflag, l_shipmode) AS m FROM online) SELECT m, m BETWEEN 0 AND 0.0001 AS ok FROM metric

-- UG-Distribution-036
WITH metric AS (SELECT CRAMERS_V(l_linestatus, l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.95 AND 1 AS ok FROM metric

-- UG-Distribution-037
WITH metric AS (SELECT MUTUAL_INFORMATION(l_linestatus, l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.62 AND 0.7 AS ok FROM metric

-- UG-Distribution-038
WITH metric AS (SELECT SPEARMAN(l_extendedprice, l_quantity) AS m FROM online) SELECT m, m BETWEEN 0.92 AND 1 AS ok FROM metric

-- UG-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM online) SELECT n, n >= 18000000 AS ok FROM metric

-- UG-Freshness-001
WITH metric AS (SELECT MAX(l_receiptdate) AS latest FROM online) SELECT latest, latest >= CAST('1998-12-01' AS DATE) AS ok FROM metric

-- UG-Freshness-002
WITH metric AS (SELECT MAX(l_shipdate) AS latest FROM online) SELECT latest, latest >= CAST('1998-11-01' AS DATE) AS ok FROM metric
