-- sidra's `workload/tpch_ml_complex_rich.yaml`: 147 data-quality checks over TPC-H, reading
-- `online` (data under test) and `training` (reference), both lineitem-shaped. One SQL statement per check.
--
-- Each check is fused into one query by sidra's `emit_sql_set.fused`: every
-- metric node becomes a CTE and the assertion node becomes the outer SELECT, so
-- its threshold travels with the query. Calls to sidra's own functions
-- (`entropy`, `cramers_v`, `tvd_similarity`, ...; `workload/udf/`) are left
-- as written: they are not DataFusion built-ins.
--
-- Regenerate with, from the sidra repo:
--   uv run python -c "import sys; sys.path.insert(0, 'pyscripts/asap-planner_test'); from emit_sql_set import fused; [print(f'-- {i}\n{s}\n') for i, _, s in fused('workload/tpch_ml_complex_rich.yaml', ['online', 'training'])[0]]"
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
WITH metric AS (SELECT COUNT(l_orderkey) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-011
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-012
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-013
WITH metric AS (SELECT COUNT(l_discount) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Completeness-014
WITH metric AS (SELECT COUNT(l_quantity) * 1.0 / COUNT(*) AS completeness FROM online) SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- UG-Uniqueness-001
WITH metric AS (SELECT COUNT(DISTINCT l_orderkey, l_linenumber) * 1.0 / COUNT(*) AS distinctness FROM online) SELECT distinctness, distinctness >= 0.999 AS ok FROM metric

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
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice / l_quantity BETWEEN 900 AND 2100 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-008
WITH metric AS (SELECT AVG(CASE WHEN l_discount BETWEEN 0 AND 0.1 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-009
WITH metric AS (SELECT AVG(CASE WHEN l_quantity BETWEEN 1 AND 50 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-010
WITH metric AS (SELECT AVG(CASE WHEN (l_linestatus = 'F') = (l_shipdate <= CAST('1995-06-17' AS DATE)) THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-011
WITH metric AS (SELECT AVG(CASE WHEN l_suppkey BETWEEN 1 AND 170000 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-012
WITH metric AS (SELECT AVG(CASE WHEN l_shipmode IN ('REG AIR', 'TRUCK', 'FOB', 'AIR', 'SHIP', 'RAIL', 'MAIL') THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Validity-013
WITH metric AS (SELECT AVG(CASE WHEN l_extendedprice > 0 THEN 1.0 ELSE 0.0 END) AS valid_rate FROM online) SELECT valid_rate, valid_rate >= 0.999 AS ok FROM metric

-- UG-Distribution-001
WITH metric AS (SELECT MIN(l_quantity) AS m FROM online) SELECT m, m >= 1 AS ok FROM metric

-- UG-Distribution-002
WITH metric AS (SELECT AVG(l_tax) AS m FROM online) SELECT m, m BETWEEN 0.039 AND 0.041 AS ok FROM metric

-- UG-Distribution-003
WITH metric AS (SELECT MAX(l_discount) AS m FROM online) SELECT m, m <= 0.1 AS ok FROM metric

-- UG-Distribution-004
WITH metric AS (SELECT VARIANCE_POP(l_quantity) AS m FROM online) SELECT m, m BETWEEN 200 AND 216 AS ok FROM metric

-- UG-Distribution-005
WITH metric AS (SELECT MIN(l_extendedprice) AS m FROM online) SELECT m, m >= 900 AS ok FROM metric

-- UG-Distribution-006
WITH metric AS (SELECT MAX(l_quantity) AS m FROM online) SELECT m, m <= 50 AS ok FROM metric

-- UG-Distribution-007
WITH metric AS (SELECT VARIANCE_POP(l_extendedprice) AS m FROM online) SELECT m, m BETWEEN 510000000 AND 575000000 AS ok FROM metric

-- UG-Distribution-008
WITH metric AS (SELECT AVG(l_quantity) AS m FROM online) SELECT m, m BETWEEN 25.2 AND 25.8 AS ok FROM metric

-- UG-Distribution-009
WITH metric AS (SELECT AVG(l_extendedprice) AS m FROM online) SELECT m, m BETWEEN 37500 AND 39000 AS ok FROM metric

-- UG-Distribution-010
WITH metric AS (SELECT AVG(l_discount) AS m FROM online) SELECT m, m BETWEEN 0.0485 AND 0.0515 AS ok FROM metric

-- UG-Distribution-011
WITH metric AS (SELECT MAX(l_extendedprice) AS m FROM online) SELECT m, m <= 105000 AS ok FROM metric

-- UG-Distribution-012
WITH metric AS (SELECT MAX(l_tax) AS m FROM online) SELECT m, m <= 0.08 AS ok FROM metric

-- UG-Distribution-013
WITH metric AS (SELECT MODE_FREQUENCY(l_linestatus) AS m FROM online) SELECT m, m BETWEEN 0.49 AND 0.51 AS ok FROM metric

-- UG-Distribution-014
WITH metric AS (SELECT GINI_IMPURITY(l_shipinstruct) AS m FROM online) SELECT m, m BETWEEN 0.74 AND 0.75 AS ok FROM metric

-- UG-Distribution-015
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM online) SELECT m, m BETWEEN 1.92 AND 1.946 AS ok FROM metric

-- UG-Distribution-016
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.5) AS m FROM online) SELECT m, m BETWEEN 35800 AND 37700 AS ok FROM metric

-- UG-Distribution-017
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM online) SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- UG-Distribution-018
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.05) AS m FROM online) SELECT m, m BETWEEN 2.5 AND 3.6 AS ok FROM metric

-- UG-Distribution-019
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.95) AS m FROM online) SELECT m, m BETWEEN 47 AND 49.5 AS ok FROM metric

-- UG-Distribution-020
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.05) AS m FROM online) SELECT m, m BETWEEN 4000 AND 4650 AS ok FROM metric

-- UG-Distribution-021
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_discount, 0.5) AS m FROM online) SELECT m, m BETWEEN 0.04 AND 0.06 AS ok FROM metric

-- UG-Distribution-022
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_extendedprice, 0.95) AS m FROM online) SELECT m, m BETWEEN 78000 AND 81000 AS ok FROM metric

-- UG-Distribution-023
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_tax, 0.95) AS m FROM online) SELECT m, m BETWEEN 0.07 AND 0.08 AS ok FROM metric

-- UG-Distribution-024
WITH metric AS (SELECT APPROX_PERCENTILE_CONT(l_quantity, 0.5) AS m FROM online) SELECT m, m BETWEEN 24 AND 27 AS ok FROM metric

-- UG-Distribution-025
WITH metric AS (SELECT ENTROPY(l_returnflag) AS m FROM online) SELECT m, m BETWEEN 1 AND 1.07 AS ok FROM metric

-- UG-Distribution-026
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.48 AND 0.53 AS ok FROM metric

-- UG-Distribution-027
WITH metric AS (SELECT CRAMERS_V(l_shipinstruct, l_shipmode) AS m FROM online) SELECT m, m BETWEEN 0 AND 0.01 AS ok FROM metric

-- UG-Distribution-028
WITH metric AS (SELECT MUTUAL_INFORMATION(l_returnflag, l_shipmode) AS m FROM online) SELECT m, m BETWEEN 0 AND 0.0001 AS ok FROM metric

-- UG-Distribution-029
WITH metric AS (SELECT CRAMERS_V(l_linestatus, l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.95 AND 1 AS ok FROM metric

-- UG-Distribution-030
WITH metric AS (SELECT MUTUAL_INFORMATION(l_linestatus, l_returnflag) AS m FROM online) SELECT m, m BETWEEN 0.62 AND 0.6932 AS ok FROM metric

-- UG-Distribution-031
WITH metric AS (SELECT SPEARMAN(l_extendedprice, l_quantity) AS m FROM online) SELECT m, m BETWEEN 0.92 AND 0.97 AS ok FROM metric

-- UG-Volume-001
WITH metric AS (SELECT COUNT(*) AS n FROM online) SELECT n, n >= 18000000 AS ok FROM metric

-- UG-Freshness-001
WITH metric AS (SELECT MAX(l_receiptdate) AS latest FROM online) SELECT latest, latest >= CAST('1998-12-01' AS DATE) AS ok FROM metric

-- UG-Freshness-002
WITH metric AS (SELECT MAX(l_shipdate) AS latest FROM online) SELECT latest, latest >= CAST('1998-11-01' AS DATE) AS ok FROM metric

-- US-Completeness-001
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'N') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-002
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'N') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-003
WITH metric AS (SELECT COUNT(l_extendedprice) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_shipmode = 'TRUCK') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-004
WITH metric AS (SELECT COUNT(l_shipinstruct) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_shipmode = 'AIR') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-005
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-006
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_linestatus = 'O') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-007
WITH metric AS (SELECT COUNT(l_returnflag) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_linestatus = 'O') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-008
WITH metric AS (SELECT COUNT(l_commitdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-009
WITH metric AS (SELECT COUNT(l_linestatus) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-010
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'R') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-011
WITH metric AS (SELECT COUNT(l_shipdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'N') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-012
WITH metric AS (SELECT COUNT(l_receiptdate) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_returnflag = 'N') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Completeness-013
WITH metric AS (SELECT COUNT(l_shipmode) * 1.0 / COUNT(*) AS completeness FROM online WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT completeness, completeness >= 0.999 AS ok FROM metric

-- US-Distribution-001
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM online WHERE l_shipmode = 'AIR') SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- US-Distribution-002
WITH metric AS (SELECT ENTROPY(l_shipmode) AS m FROM online WHERE l_shipinstruct = 'DELIVER IN PERSON') SELECT m, m BETWEEN 1.93 AND 1.946 AS ok FROM metric

-- US-Distribution-003
WITH metric AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM online WHERE l_linestatus = 'O') SELECT m, m BETWEEN 0.99 AND 1 AS ok FROM metric

-- US-Distribution-004
WITH metric AS (SELECT GINI_IMPURITY(l_linestatus) AS m FROM online WHERE l_returnflag = 'N') SELECT m, m BETWEEN 0.015 AND 0.04 AS ok FROM metric

-- US-Distribution-005
WITH metric AS (SELECT ENTROPY(l_shipinstruct) AS m FROM online WHERE l_linestatus = 'O') SELECT m, m BETWEEN 1.37 AND 1.3863 AS ok FROM metric

-- US-Distribution-006
WITH metric AS (SELECT GINI_IMPURITY(l_linestatus) AS m FROM online WHERE l_returnflag = 'R') SELECT m, m BETWEEN 0 AND 0.001 AS ok FROM metric

-- BG-Distribution-001
WITH m_a AS (SELECT ENTROPY(l_returnflag) AS m FROM online), m_b AS (SELECT ENTROPY(l_returnflag) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.003 AS ok FROM m_a, m_b

-- BG-Distribution-002
WITH m_a AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM online), m_b AS (SELECT MODE_FREQUENCY(l_returnflag) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.003 AS ok FROM m_a, m_b

-- BG-Distribution-003
WITH m_a AS (SELECT SKEWNESS(l_quantity) AS m FROM online), m_b AS (SELECT SKEWNESS(l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-004
WITH m_a AS (SELECT AVG(l_extendedprice) AS m FROM online), m_b AS (SELECT AVG(l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 60 AS ok FROM m_a, m_b

-- BG-Distribution-005
WITH m_a AS (SELECT IQR(l_quantity) AS m FROM online), m_b AS (SELECT IQR(l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 1 AS ok FROM m_a, m_b

-- BG-Distribution-006
WITH m_a AS (SELECT STDDEV_POP(l_extendedprice) AS m FROM online), m_b AS (SELECT STDDEV_POP(l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 80 AS ok FROM m_a, m_b

-- BG-Distribution-007
WITH m_a AS (SELECT AVG(l_quantity) AS m FROM online), m_b AS (SELECT AVG(l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.04 AS ok FROM m_a, m_b

-- BG-Distribution-008
WITH m_a AS (SELECT STDDEV_POP(l_tax) AS m FROM online), m_b AS (SELECT STDDEV_POP(l_tax) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.0001 AS ok FROM m_a, m_b

-- BG-Distribution-009
WITH m_a AS (SELECT AVG(l_discount) AS m FROM online), m_b AS (SELECT AVG(l_discount) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.00015 AS ok FROM m_a, m_b

-- BG-Distribution-010
WITH m_a AS (SELECT ENTROPY(l_linestatus) AS m FROM online), m_b AS (SELECT ENTROPY(l_linestatus) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.0001 AS ok FROM m_a, m_b

-- BG-Distribution-011
WITH m_a AS (SELECT TOP_K_SHARE(l_shipmode, 5) AS m FROM online), m_b AS (SELECT TOP_K_SHARE(l_shipmode, 5) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.002 AS ok FROM m_a, m_b

-- BG-Distribution-012
WITH m_a AS (SELECT CV(l_extendedprice) AS m FROM online), m_b AS (SELECT CV(l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.003 AS ok FROM m_a, m_b

-- BG-Distribution-013
WITH m_a AS (SELECT ENTROPY(l_shipinstruct) AS m FROM online), m_b AS (SELECT ENTROPY(l_shipinstruct) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 1e-05 AS ok FROM m_a, m_b

-- BG-Distribution-014
WITH m_a AS (SELECT AVG(l_tax) AS m FROM online), m_b AS (SELECT AVG(l_tax) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.00015 AS ok FROM m_a, m_b

-- BG-Distribution-015
WITH m_a AS (SELECT STDDEV_POP(l_quantity) AS m FROM online), m_b AS (SELECT STDDEV_POP(l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.02 AS ok FROM m_a, m_b

-- BG-Distribution-016
WITH m_a AS (SELECT GINI_IMPURITY(l_shipmode) AS m FROM online), m_b AS (SELECT GINI_IMPURITY(l_shipmode) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 1e-05 AS ok FROM m_a, m_b

-- BG-Distribution-017
WITH m_a AS (SELECT SKEWNESS(l_extendedprice) AS m FROM online), m_b AS (SELECT SKEWNESS(l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.01 AS ok FROM m_a, m_b

-- BG-Distribution-018
WITH m_a AS (SELECT ENTROPY(l_shipmode) AS m FROM online), m_b AS (SELECT ENTROPY(l_shipmode) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 1e-05 AS ok FROM m_a, m_b

-- BG-Distribution-019
WITH m_a AS (SELECT IQR(l_extendedprice) AS m FROM online), m_b AS (SELECT IQR(l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 200 AS ok FROM m_a, m_b

-- BG-Distribution-020
WITH m_a AS (SELECT STDDEV_POP(l_discount) AS m FROM online), m_b AS (SELECT STDDEV_POP(l_discount) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.0001 AS ok FROM m_a, m_b

-- BG-Distribution-021
WITH m_a AS (SELECT MUTUAL_INFORMATION(l_linestatus, l_returnflag) AS m FROM online), m_b AS (SELECT MUTUAL_INFORMATION(l_linestatus, l_returnflag) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.003 AS ok FROM m_a, m_b

-- BG-Distribution-022
WITH m_a AS (SELECT SPEARMAN(l_discount, l_extendedprice) AS m FROM online), m_b AS (SELECT SPEARMAN(l_discount, l_extendedprice) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-023
WITH m_a AS (SELECT CRAMERS_V(l_linestatus, l_returnflag) AS m FROM online), m_b AS (SELECT CRAMERS_V(l_linestatus, l_returnflag) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-024
WITH m_a AS (SELECT SPEARMAN(l_extendedprice, l_quantity) AS m FROM online), m_b AS (SELECT SPEARMAN(l_extendedprice, l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-025
WITH m_a AS (SELECT CORR(l_extendedprice, l_quantity) AS m FROM online), m_b AS (SELECT CORR(l_extendedprice, l_quantity) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-026
WITH m_a AS (SELECT CORR(l_discount, l_tax) AS m FROM online), m_b AS (SELECT CORR(l_discount, l_tax) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-027
WITH m_a AS (SELECT CRAMERS_V(l_shipinstruct, l_shipmode) AS m FROM online), m_b AS (SELECT CRAMERS_V(l_shipinstruct, l_shipmode) AS m FROM training) SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.005 AS ok FROM m_a, m_b

-- BG-Distribution-028
WITH score AS (SELECT TVD_SIMILARITY_BINNED(online, (l_tax), training, (l_tax), 10, 0.0, 0.008) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-029
WITH score AS (SELECT COHENS_D(online, (l_discount), training, (l_discount)) AS score) SELECT score, ABS(score) <= 0.05 AS ok FROM score

-- BG-Distribution-030
WITH score AS (SELECT WELCH_T(online, (l_extendedprice), training, (l_extendedprice)) AS score) SELECT score, ABS(score) <= 4 AS ok FROM score

-- BG-Distribution-031
WITH score AS (SELECT COHENS_D(online, (l_tax), training, (l_tax)) AS score) SELECT score, ABS(score) <= 0.05 AS ok FROM score

-- BG-Distribution-032
WITH score AS (SELECT COHENS_D(online, (l_extendedprice), training, (l_extendedprice)) AS score) SELECT score, ABS(score) <= 0.05 AS ok FROM score

-- BG-Distribution-033
WITH score AS (SELECT WELCH_T(online, (l_quantity), training, (l_quantity)) AS score) SELECT score, ABS(score) <= 4 AS ok FROM score

-- BG-Distribution-034
WITH score AS (SELECT JS_SIMILARITY_BINNED(online, (l_extendedprice), training, (l_extendedprice), 10, 901.91, 10404.009) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-035
WITH score AS (SELECT TVD_SIMILARITY_BINNED(online, (l_discount), training, (l_discount), 10, 0.0, 0.01) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-036
WITH score AS (SELECT TVD_SIMILARITY_BINNED(online, (l_quantity), training, (l_quantity), 10, 1.0, 4.9) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-037
WITH a_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM online GROUP BY l_returnflag), b_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM training GROUP BY l_returnflag), score AS (SELECT HELLINGER_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-038
WITH a_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM online GROUP BY l_shipmode), b_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM training GROUP BY l_shipmode), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-039
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training GROUP BY l_shipinstruct), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-040
WITH a_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM online GROUP BY l_shipmode), b_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM training GROUP BY l_shipmode), score AS (SELECT JS_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-041
WITH a_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM online GROUP BY l_linestatus), b_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM training GROUP BY l_linestatus), score AS (SELECT LINF_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-042
WITH a_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM online GROUP BY l_returnflag), b_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM training GROUP BY l_returnflag), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-043
WITH a_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM online GROUP BY l_linestatus), b_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM training GROUP BY l_linestatus), score AS (SELECT JS_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-044
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training GROUP BY l_shipinstruct), score AS (SELECT LINF_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-045
WITH score AS (SELECT GENERALIZED_JACCARD(online, (l_linestatus), training, (l_linestatus)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-046
WITH score AS (SELECT GENERALIZED_JACCARD(online, (l_shipmode), training, (l_shipmode)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-047
WITH score AS (SELECT GENERALIZED_JACCARD(online, (l_shipinstruct), training, (l_shipinstruct)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Distribution-048
WITH score AS (SELECT GENERALIZED_JACCARD(online, (l_returnflag), training, (l_returnflag)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BG-Referential-Integrity-001
WITH metric AS (SELECT COUNT(b.l_suppkey) * 1.0 / COUNT(*) AS matched FROM online AS a LEFT JOIN training AS b ON a.l_suppkey = b.l_suppkey WHERE a.l_suppkey IS NOT NULL) SELECT matched, matched >= 0.999 AS ok FROM metric

-- BG-Referential-Integrity-002
WITH metric AS (SELECT COUNT(b.l_partkey) * 1.0 / COUNT(*) AS matched FROM online AS a LEFT JOIN training AS b ON a.l_partkey = b.l_partkey WHERE a.l_partkey IS NOT NULL) SELECT matched, matched >= 0.999 AS ok FROM metric

-- BG-Volume-001
WITH metric AS (SELECT (SELECT COUNT(*) FROM online) * 1.0 / NULLIF((SELECT COUNT(*) FROM training), 0) AS ratio) SELECT ratio, ratio BETWEEN 0.36 AND 0.44 AS ok FROM metric

-- BS-Distribution-001
WITH m_a AS (SELECT ENTROPY(l_returnflag) AS m FROM online WHERE l_linestatus = 'O'), m_b AS (SELECT ENTROPY(l_returnflag) AS m FROM training WHERE l_linestatus = 'O') SELECT m_a.m AS m_a, m_b.m AS m_b, ABS(m_a.m - m_b.m) AS diff, ABS(m_a.m - m_b.m) <= 0.01 AS ok FROM m_a, m_b

-- BS-Distribution-002
WITH a_s AS (SELECT * FROM online WHERE l_shipmode = 'TRUCK'), b_s AS (SELECT * FROM training WHERE l_shipmode = 'TRUCK'), score AS (SELECT JS_SIMILARITY_BINNED(a_s, (l_extendedprice), b_s, (l_extendedprice), 10, 901.91, 10404.009) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-003
WITH a_s AS (SELECT * FROM online WHERE l_shipmode = 'TRUCK'), b_s AS (SELECT * FROM training WHERE l_shipmode = 'TRUCK'), score AS (SELECT TVD_SIMILARITY_BINNED(a_s, (l_quantity), b_s, (l_quantity), 10, 1.0, 4.9) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-004
WITH a_s AS (SELECT * FROM online WHERE l_linenumber = 1), b_s AS (SELECT * FROM training WHERE l_linenumber = 1), score AS (SELECT TVD_SIMILARITY_BINNED(a_s, (l_quantity), b_s, (l_quantity), 10, 1.0, 4.9) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-005
WITH a_s AS (SELECT * FROM online WHERE l_linenumber = 1), b_s AS (SELECT * FROM training WHERE l_linenumber = 1), score AS (SELECT JS_SIMILARITY_BINNED(a_s, (l_extendedprice), b_s, (l_extendedprice), 10, 901.91, 10404.009) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-006
WITH a_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM online WHERE l_shipinstruct = 'DELIVER IN PERSON' GROUP BY l_shipmode), b_f AS (SELECT l_shipmode AS k, COUNT(*) AS n FROM training WHERE l_shipinstruct = 'DELIVER IN PERSON' GROUP BY l_shipmode), score AS (SELECT LINF_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-007
WITH a_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM online WHERE l_returnflag = 'R' GROUP BY l_linestatus), b_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM training WHERE l_returnflag = 'R' GROUP BY l_linestatus), score AS (SELECT JS_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-008
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online WHERE l_linenumber = 1 GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training WHERE l_linenumber = 1 GROUP BY l_shipinstruct), score AS (SELECT HELLINGER_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-009
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online WHERE l_returnflag = 'N' GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training WHERE l_returnflag = 'N' GROUP BY l_shipinstruct), score AS (SELECT HELLINGER_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-010
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online WHERE l_shipmode = 'AIR' GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training WHERE l_shipmode = 'AIR' GROUP BY l_shipinstruct), score AS (SELECT HELLINGER_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-011
WITH a_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM online WHERE l_linestatus = 'O' GROUP BY l_shipinstruct), b_f AS (SELECT l_shipinstruct AS k, COUNT(*) AS n FROM training WHERE l_linestatus = 'O' GROUP BY l_shipinstruct), score AS (SELECT HELLINGER_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-012
WITH a_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM online WHERE l_linestatus = 'O' GROUP BY l_returnflag), b_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM training WHERE l_linestatus = 'O' GROUP BY l_returnflag), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-013
WITH a_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM online WHERE l_linenumber = 1 GROUP BY l_returnflag), b_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM training WHERE l_linenumber = 1 GROUP BY l_returnflag), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-014
WITH a_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM online WHERE l_returnflag = 'N' GROUP BY l_linestatus), b_f AS (SELECT l_linestatus AS k, COUNT(*) AS n FROM training WHERE l_returnflag = 'N' GROUP BY l_linestatus), score AS (SELECT JS_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score

-- BS-Distribution-015
WITH a_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM online WHERE l_shipmode = 'TRUCK' GROUP BY l_returnflag), b_f AS (SELECT l_returnflag AS k, COUNT(*) AS n FROM training WHERE l_shipmode = 'TRUCK' GROUP BY l_returnflag), score AS (SELECT TVD_SIMILARITY(a_f, (k), (n), b_f, (k), (n)) AS score) SELECT score, score >= 0.95 AS ok FROM score
