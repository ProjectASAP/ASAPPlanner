-- sidra's `workload/tpch_gate_distribution_rich.yaml`: 78 data-quality checks over TPC-H, reading
-- `lineitem`. One SQL statement per check.
--
-- Each check is fused into one query by sidra's `emit_sql_set.fused`: every
-- metric node becomes a CTE and the assertion node becomes the outer SELECT, so
-- its threshold travels with the query. Calls to sidra's own functions
-- (`entropy`, `cramers_v`, `tvd_similarity`, ...; `workload/udf/`) are left
-- as written: they are not DataFusion built-ins.
--
-- Regenerate with, from the sidra repo:
--   uv run python -c "import sys; sys.path.insert(0, 'pyscripts/asap-planner_test'); from emit_sql_set import fused; [print(f'-- {i}\n{s}\n') for i, _, s in fused('workload/tpch_gate_distribution_rich.yaml', ['lineitem'])[0]]"
--
-- ONE QUERY PER LINE: `-- <test id>` then the query. The reader splits on
-- newlines, not on `;`.

-- UG-Completeness-001
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-002
WITH metric AS (SELECT COUNT(l_suppkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-003
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-004
WITH metric AS (SELECT COUNT(l_tax) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-005
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-006
WITH metric AS (SELECT COUNT(l_extendedprice) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-007
WITH metric AS (SELECT COUNT(l_partkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-008
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-009
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 1 AS ok FROM metric

-- UG-Completeness-010
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-011
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-012
WITH metric AS (SELECT COUNT(l_discount) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-013
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS completeness FROM lineitem) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Uniqueness-001
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS distinctness FROM lineitem) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

-- UG-Validity-001
WITH metric AS (SELECT AVG(CASE WHEN l_linestatus IN ('F', 'O') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-002
WITH metric AS (SELECT AVG(CASE WHEN l_linenumber BETWEEN 1 AND 7 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-003
WITH metric AS (SELECT AVG(CASE WHEN l_shipinstruct IN ('NONE', 'DELIVER IN PERSON', 'COLLECT COD', 'TAKE BACK RETURN') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-004
WITH metric AS (SELECT AVG(CASE WHEN l_shipdate <= l_receiptdate THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-005
WITH metric AS (SELECT AVG(CASE WHEN l_returnflag IN ('N', 'R', 'A') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-006
WITH metric AS (SELECT AVG(CASE WHEN l_tax BETWEEN 0 AND 0.08 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-007
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice BETWEEN 900 AND 105000 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0 AND 0.1 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM lineitem) SELECT valid_rate, valid_rate >= 1 AS ok FROM metric

-- UG-Validity-010
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
WITH metric AS (SELECT AVG(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 25.3 AND 25.7 AS ok FROM metric

-- UG-Distribution-008
WITH metric AS (SELECT STDDEV_POP(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 14.2 AND 14.65 AS ok FROM metric

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

-- UG-Distribution-015
WITH metric AS (SELECT MODE_FREQUENCY(l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.49 AND 0.51 AS ok FROM metric

-- UG-Distribution-016
WITH metric AS (SELECT GINI_IMPURITY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 0.74 AND 0.755 AS ok FROM metric

-- UG-Distribution-017
WITH metric AS (SELECT MODE_FREQUENCY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0.14 AND 0.147 AS ok FROM metric

-- UG-Distribution-018
WITH metric AS (SELECT SKEWNESS(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- UG-Distribution-019
WITH metric AS (SELECT IQR(l_discount) AS m FROM lineitem) SELECT m, m BETWEEN 0.05 AND 0.07 AS ok FROM metric

-- UG-Distribution-020
WITH metric AS (SELECT IQR(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN 24 AND 26 AS ok FROM metric

-- UG-Distribution-021
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 1.93 AND 1.9461 AS ok FROM metric

-- UG-Distribution-022
WITH metric AS (SELECT IQR(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 35000 AND 37800 AS ok FROM metric

-- UG-Distribution-023
WITH metric AS (SELECT SKEWNESS(l_tax) AS m FROM lineitem) SELECT m, m BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- UG-Distribution-024
WITH metric AS (SELECT PERCENTILE_CONT(l_quantity, 0.05) AS m FROM lineitem) SELECT m, m BETWEEN 2 AND 4 AS ok FROM metric

-- UG-Distribution-025
WITH metric AS (SELECT IQR(l_tax) AS m FROM lineitem) SELECT m, m BETWEEN 0.03 AND 0.05 AS ok FROM metric

-- UG-Distribution-026
WITH metric AS (SELECT GINI_IMPURITY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 0.59 AND 0.65 AS ok FROM metric

-- UG-Distribution-027
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- UG-Distribution-028
WITH metric AS (SELECT SKEWNESS(l_discount) AS m FROM lineitem) SELECT m, m BETWEEN -0.05 AND 0.05 AS ok FROM metric

-- UG-Distribution-029
WITH metric AS (SELECT GINI_IMPURITY(l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.48 AND 0.5 AS ok FROM metric

-- UG-Distribution-030
WITH metric AS (SELECT PERCENTILE_CONT(l_discount, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 0.04 AND 0.06 AS ok FROM metric

-- UG-Distribution-031
WITH metric AS (SELECT KURTOSIS(l_quantity) AS m FROM lineitem) SELECT m, m BETWEEN -1.3 AND -1.1 AS ok FROM metric

-- UG-Distribution-032
WITH metric AS (SELECT PERCENTILE_CONT(l_quantity, 0.95) AS m FROM lineitem) SELECT m, m BETWEEN 47 AND 49 AS ok FROM metric

-- UG-Distribution-033
WITH metric AS (SELECT PERCENTILE_CONT(l_extendedprice, 0.95) AS m FROM lineitem) SELECT m, m BETWEEN 76000 AND 83000 AS ok FROM metric

-- UG-Distribution-034
WITH metric AS (SELECT PERCENTILE_CONT(l_tax, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 0.03 AND 0.05 AS ok FROM metric

-- UG-Distribution-035
WITH metric AS (SELECT OUTLIER_RATIO(l_extendedprice, 3) AS m FROM lineitem) SELECT m, m BETWEEN 0 AND 0.001 AS ok FROM metric

-- UG-Distribution-036
WITH metric AS (SELECT ENTROPY(l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.68 AND 0.6932 AS ok FROM metric

-- UG-Distribution-037
WITH metric AS (SELECT PERCENTILE_CONT(l_extendedprice, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 35000 AND 38500 AS ok FROM metric

-- UG-Distribution-038
WITH metric AS (SELECT KURTOSIS(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN -0.85 AND -0.65 AS ok FROM metric

-- UG-Distribution-039
WITH metric AS (SELECT GINI_IMPURITY(l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0.85 AND 0.858 AS ok FROM metric

-- UG-Distribution-040
WITH metric AS (SELECT ENTROPY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 1.01 AND 1.06 AS ok FROM metric

-- UG-Distribution-041
WITH metric AS (SELECT TOP_K_SHARE(l_shipmode, 5) AS m FROM lineitem) SELECT m, m BETWEEN 0.69 AND 0.74 AS ok FROM metric

-- UG-Distribution-042
WITH metric AS (SELECT MODE_FREQUENCY(l_shipinstruct) AS m FROM lineitem) SELECT m, m BETWEEN 0.24 AND 0.26 AS ok FROM metric

-- UG-Distribution-043
WITH metric AS (SELECT PERCENTILE_CONT(l_quantity, 0.5) AS m FROM lineitem) SELECT m, m BETWEEN 24 AND 26 AS ok FROM metric

-- UG-Distribution-044
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM lineitem) SELECT m, m BETWEEN 0.49 AND 0.52 AS ok FROM metric

-- UG-Distribution-045
WITH metric AS (SELECT SKEWNESS(l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 0.28 AND 0.38 AS ok FROM metric

-- UG-Distribution-046
WITH metric AS (SELECT PERCENTILE_CONT(l_extendedprice, 0.05) AS m FROM lineitem) SELECT m, m BETWEEN 4000 AND 4600 AS ok FROM metric

-- UG-Distribution-047
WITH metric AS (SELECT CORR(l_quantity, l_extendedprice) AS m FROM lineitem) SELECT m, m BETWEEN 0.9 AND 0.96 AS ok FROM metric

-- UG-Distribution-048
WITH metric AS (SELECT CORR(l_discount, l_tax) AS m FROM lineitem) SELECT m, m BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- UG-Distribution-049
WITH metric AS (SELECT SPEARMAN(l_quantity, l_discount) AS m FROM lineitem) SELECT m, m BETWEEN -0.02 AND 0.02 AS ok FROM metric

-- UG-Distribution-050
WITH metric AS (SELECT MUTUAL_INFORMATION(l_shipinstruct, l_shipmode) AS m FROM lineitem) SELECT m, m BETWEEN 0 AND 0.0001 AS ok FROM metric

-- UG-Distribution-051
WITH metric AS (SELECT CRAMERS_V(l_returnflag, l_linestatus) AS m FROM lineitem) SELECT m, m BETWEEN 0.97 AND 1 AS ok FROM metric
