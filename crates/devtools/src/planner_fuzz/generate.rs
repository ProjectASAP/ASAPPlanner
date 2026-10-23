//! Random small workloads: structured query specs rendered to PromQL or
//! SQL, the data facts they read, and a seeded RNG so a run is reproducible
//! from its seed.

use serde::{Deserialize, Serialize};
use serde_json::json;

use asap_types::ir::properties::AccuracyTarget;

use super::{FuzzQuery, FuzzWorkload, Language};

/// SplitMix64: a small, fast, seedable generator. Deterministic across
/// platforms, so `--seed N` reproduces a run.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

/// One generated query in structured form, so relations can derive a
/// related query (a filter, a coarser grouping, a shorter window, another
/// quantile rank) and the shrinker can simplify it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuerySpec {
    Promql(PromqlSpec),
    Sql(SqlSpec),
}

/// `outer(range(metric{matchers}[window_ms]))`; without `range`, an instant
/// selector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromqlSpec {
    pub metric: String,
    /// Equality label matchers on the selector, `(label, value)`.
    pub matchers: Vec<(String, String)>,
    pub range: Option<RangeFunction>,
    /// The range selector's window, in milliseconds; ignored without `range`.
    pub window_ms: u64,
    pub outer: Option<OuterAggregation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeFunction {
    Rate,
    Increase,
    SumOverTime,
    CountOverTime,
    AvgOverTime,
    MaxOverTime,
    QuantileOverTime(f64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OuterAggregation {
    pub op: OuterOp,
    /// The `by` labels, or the `without` labels when `without`; empty with
    /// `by` aggregates everything into one group.
    pub labels: Vec<String>,
    pub without: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OuterOp {
    Sum,
    Count,
    Max,
    Min,
    Avg,
    Quantile(f64),
    Topk(u32),
}

/// `SELECT group_by.., aggregate AS v FROM table WHERE ts > now() - window
/// AND filters.. GROUP BY group_by..`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SqlSpec {
    pub table: String,
    pub aggregate: SqlAggregate,
    pub group_by: Vec<String>,
    /// Equality filters, `(column, value)`.
    pub filters: Vec<(String, String)>,
    pub window_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlAggregate {
    Sum,
    Count,
    Avg,
    Max,
    Min,
    Quantile(f64),
    CountDistinct(String),
}

/// The PromQL metrics the generator reads, with their type.
pub const GAUGE_METRIC: &str = "m_gauge";
pub const COUNTER_METRIC: &str = "m_counter";
/// Every generated PromQL series carries these labels, with this many
/// distinct values each (`label="<label><n>"`, `n` in `1..=distinct`).
pub const PROMQL_LABELS: [(&str, u64); 3] = [("job", 5), ("instance", 50), ("region", 3)];
/// The SQL table the generator reads and its string columns with their
/// distinct value counts; `value` is the measured float column.
pub const SQL_TABLE: &str = "metrics";
pub const SQL_COLUMNS: [(&str, u64); 2] = [("city", 10), ("host", 100)];

const WINDOWS_MS: [u64; 4] = [60_000, 120_000, 300_000, 600_000];
const REPEAT_INTERVALS_MS: [u64; 3] = [10_000, 30_000, 60_000];
const QUANTILES: [f64; 4] = [0.5, 0.9, 0.95, 0.99];
const SERIES_COUNTS: [u64; 3] = [100, 1_000, 10_000];
const SAMPLE_INTERVALS_MS: [u64; 2] = [10_000, 15_000];
const LATENCY_BOUNDS_MS: [f64; 2] = [50.0, 1_000.0];

/// The accuracy targets the generator draws from, tightest first: exact,
/// then ε = 0.01 with δ = 0.001, ε = 0.05 with δ = 0.01, and ε = 0.1 with
/// no δ (the loosest).
pub fn accuracy_ladder() -> [AccuracyTarget; 4] {
    [
        AccuracyTarget::exact(),
        AccuracyTarget::max_error(0.01).failure_probability(0.001),
        AccuracyTarget::max_error(0.05).failure_probability(0.01),
        AccuracyTarget::max_error(0.1),
    ]
}

/// The next looser target than `target` on [`accuracy_ladder`], or `None`
/// at the loosest. A target not on the ladder loosens to ε = 0.1.
pub fn looser(target: &AccuracyTarget) -> Option<AccuracyTarget> {
    let ladder = accuracy_ladder();
    match ladder.iter().position(|t| t == target) {
        Some(i) if i + 1 < ladder.len() => Some(ladder[i + 1].clone()),
        Some(_) => None,
        None if target.max_error < 0.1 => Some(ladder[3].clone()),
        None => None,
    }
}

/// The next tighter target than `target` on [`accuracy_ladder`], or `None`
/// when it is exact. A target not on the ladder tightens to exact.
pub fn tighter(target: &AccuracyTarget) -> Option<AccuracyTarget> {
    let ladder = accuracy_ladder();
    match ladder.iter().position(|t| t == target) {
        Some(0) => None,
        Some(i) => Some(ladder[i - 1].clone()),
        None if target.is_exact() => None,
        None => Some(AccuracyTarget::exact()),
    }
}

impl PromqlSpec {
    pub fn render(&self) -> String {
        let selector = if self.matchers.is_empty() {
            self.metric.clone()
        } else {
            let matchers: Vec<String> = self
                .matchers
                .iter()
                .map(|(label, value)| format!("{label}=\"{value}\""))
                .collect();
            format!("{}{{{}}}", self.metric, matchers.join(","))
        };
        let inner = match self.range {
            None => selector,
            Some(function) => {
                let range = format!("{selector}[{}]", duration_text(self.window_ms));
                match function {
                    RangeFunction::QuantileOverTime(q) => {
                        format!("quantile_over_time({q}, {range})")
                    }
                    other => format!("{}({range})", range_function_name(other)),
                }
            }
        };
        match &self.outer {
            None => inner,
            Some(outer) => {
                let grouping = match (outer.without, outer.labels.is_empty()) {
                    (false, true) => String::new(),
                    (false, false) => format!(" by ({})", outer.labels.join(", ")),
                    (true, _) => format!(" without ({})", outer.labels.join(", ")),
                };
                match outer.op {
                    OuterOp::Quantile(q) => format!("quantile{grouping} ({q}, {inner})"),
                    OuterOp::Topk(k) => format!("topk{grouping} ({k}, {inner})"),
                    op => format!("{}{grouping} ({inner})", outer_op_name(op)),
                }
            }
        }
    }

    /// The labels of the output: the `by` labels, every label but the
    /// `without` ones, or every label for a per-series result (whose labels
    /// are in its series identity, not columns).
    pub fn group_labels(&self) -> Vec<String> {
        match &self.outer {
            Some(outer) if !outer.without => outer.labels.clone(),
            Some(outer) => PROMQL_LABELS
                .iter()
                .map(|(label, _)| label.to_string())
                .filter(|label| !outer.labels.contains(label))
                .collect(),
            None => Vec::new(),
        }
    }
}

impl SqlSpec {
    pub fn render(&self) -> String {
        let aggregate = match &self.aggregate {
            SqlAggregate::Sum => "SUM(value)".to_string(),
            SqlAggregate::Count => "COUNT(*)".to_string(),
            SqlAggregate::Avg => "AVG(value)".to_string(),
            SqlAggregate::Max => "MAX(value)".to_string(),
            SqlAggregate::Min => "MIN(value)".to_string(),
            SqlAggregate::Quantile(q) => {
                format!("percentile_cont({q}) WITHIN GROUP (ORDER BY value)")
            }
            SqlAggregate::CountDistinct(column) => format!("COUNT(DISTINCT {column})"),
        };
        let mut select: Vec<String> = self.group_by.clone();
        select.push(format!("{aggregate} AS v"));
        let mut text = format!(
            "SELECT {} FROM {} WHERE ts > now() - {}",
            select.join(", "),
            self.table,
            sql_interval(self.window_ms)
        );
        for (column, value) in &self.filters {
            text.push_str(&format!(" AND {column} = '{value}'"));
        }
        if !self.group_by.is_empty() {
            text.push_str(&format!(" GROUP BY {}", self.group_by.join(", ")));
        }
        text
    }
}

impl QuerySpec {
    pub fn render(&self) -> String {
        match self {
            QuerySpec::Promql(spec) => spec.render(),
            QuerySpec::Sql(spec) => spec.render(),
        }
    }
}

fn range_function_name(function: RangeFunction) -> &'static str {
    match function {
        RangeFunction::Rate => "rate",
        RangeFunction::Increase => "increase",
        RangeFunction::SumOverTime => "sum_over_time",
        RangeFunction::CountOverTime => "count_over_time",
        RangeFunction::AvgOverTime => "avg_over_time",
        RangeFunction::MaxOverTime => "max_over_time",
        RangeFunction::QuantileOverTime(_) => "quantile_over_time",
    }
}

fn outer_op_name(op: OuterOp) -> &'static str {
    match op {
        OuterOp::Sum => "sum",
        OuterOp::Count => "count",
        OuterOp::Max => "max",
        OuterOp::Min => "min",
        OuterOp::Avg => "avg",
        OuterOp::Quantile(_) => "quantile",
        OuterOp::Topk(_) => "topk",
    }
}

/// PromQL duration text: whole minutes as `Nm`, else seconds as `Ns`.
pub fn duration_text(ms: u64) -> String {
    if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else {
        format!("{}s", ms / 1_000)
    }
}

fn sql_interval(ms: u64) -> String {
    if ms.is_multiple_of(60_000) {
        format!("INTERVAL '{}' MINUTE", ms / 60_000)
    } else {
        format!("INTERVAL '{}' SECOND", ms / 1_000)
    }
}

/// The SQL table declarations (`stage_pipeline --table` form) for `table`.
pub fn sql_table_declaration(table: &str, columns: &[&str]) -> serde_json::Value {
    let mut fields = vec![json!({"name": "ts", "type": "timestamp", "nullable": false})];
    for column in columns {
        fields.push(json!({"name": column, "type": "utf8", "nullable": false}));
    }
    fields.push(json!({"name": "value", "type": "float64", "nullable": false}));
    json!({"name": table, "columns": fields, "time_index": 0})
}

/// The facts of one generated source: `series_count` series sampled every
/// `sample_interval_ms`, with these labels' distinct value counts.
pub fn source_facts(
    series_count: u64,
    sample_interval_ms: u64,
    metric_type: Option<&str>,
    labels: &[(String, u64)],
) -> serde_json::Value {
    let distinct: serde_json::Map<String, serde_json::Value> = labels
        .iter()
        .map(|(label, n)| (label.clone(), json!(n.min(&series_count))))
        .collect();
    let mut facts = json!({
        "series_count": series_count,
        "sample_interval": sample_interval_ms,
        "values_non_negative": true,
        "distinct_values_per_label": distinct,
    });
    if let Some(metric_type) = metric_type {
        facts["metric_type"] = json!(metric_type);
    }
    facts
}

fn random_promql_spec(rng: &mut Rng) -> PromqlSpec {
    let counter = rng.chance(0.4);
    let metric = if counter {
        COUNTER_METRIC
    } else {
        GAUGE_METRIC
    }
    .to_string();
    let range = if counter {
        Some(*rng.pick(&[RangeFunction::Rate, RangeFunction::Increase]))
    } else if rng.chance(0.25) {
        None
    } else {
        let q = *rng.pick(&QUANTILES);
        Some(*rng.pick(&[
            RangeFunction::SumOverTime,
            RangeFunction::CountOverTime,
            RangeFunction::AvgOverTime,
            RangeFunction::MaxOverTime,
            RangeFunction::QuantileOverTime(q),
        ]))
    };
    let matchers = if rng.chance(0.4) {
        let (label, distinct) = *rng.pick(&PROMQL_LABELS);
        vec![(
            label.to_string(),
            label_value(label, rng.below(distinct as usize)),
        )]
    } else {
        Vec::new()
    };
    let outer = if rng.chance(0.75) {
        let q = *rng.pick(&QUANTILES);
        let k = *rng.pick(&[3, 10]);
        let op = *rng.pick(&[
            OuterOp::Sum,
            OuterOp::Sum,
            OuterOp::Count,
            OuterOp::Max,
            OuterOp::Min,
            OuterOp::Avg,
            OuterOp::Quantile(q),
            OuterOp::Quantile(q),
            OuterOp::Topk(k),
        ]);
        let mut labels: Vec<String> = PROMQL_LABELS
            .iter()
            .filter(|_| rng.chance(0.5))
            .map(|(label, _)| label.to_string())
            .collect();
        // Rare: the executor does not compile `without` yet.
        let without = !labels.is_empty() && rng.chance(0.05);
        if matches!(op, OuterOp::Topk(_)) && rng.chance(0.5) {
            labels.clear();
        }
        Some(OuterAggregation {
            op,
            labels,
            without,
        })
    } else {
        None
    };
    PromqlSpec {
        metric,
        matchers,
        range,
        window_ms: *rng.pick(&WINDOWS_MS),
        outer,
    }
}

fn random_sql_spec(rng: &mut Rng) -> SqlSpec {
    let q = *rng.pick(&QUANTILES);
    let aggregate = rng
        .pick(&[
            SqlAggregate::Sum,
            SqlAggregate::Count,
            SqlAggregate::Avg,
            SqlAggregate::Max,
            SqlAggregate::Min,
            SqlAggregate::Quantile(q),
            SqlAggregate::Quantile(0.9),
            SqlAggregate::CountDistinct("host".into()),
        ])
        .clone();
    let group_by: Vec<String> = SQL_COLUMNS
        .iter()
        .filter(|(column, _)| {
            rng.chance(0.45) && !matches!(&aggregate, SqlAggregate::CountDistinct(c) if c == column)
        })
        .map(|(column, _)| column.to_string())
        .collect();
    let filters = if rng.chance(0.4) {
        let (column, distinct) = *rng.pick(&SQL_COLUMNS);
        vec![(
            column.to_string(),
            label_value(column, rng.below(distinct as usize)),
        )]
    } else {
        Vec::new()
    };
    SqlSpec {
        table: SQL_TABLE.into(),
        aggregate,
        group_by,
        filters,
        window_ms: *rng.pick(&WINDOWS_MS),
    }
}

/// `label="<label><n+1>"`: the generated value names of a label.
pub fn label_value(label: &str, n: usize) -> String {
    format!("{label}{}", n + 1)
}

/// A related query: a small edit of `spec` (another operator, grouping,
/// window or quantile), so a workload holds queries that could share.
fn related_spec(rng: &mut Rng, spec: &QuerySpec) -> QuerySpec {
    let mut spec = spec.clone();
    match &mut spec {
        QuerySpec::Promql(p) => match rng.below(4) {
            0 => p.window_ms = *rng.pick(&WINDOWS_MS),
            1 => {
                if let Some(outer) = &mut p.outer {
                    outer.labels = PROMQL_LABELS
                        .iter()
                        .filter(|_| rng.chance(0.5))
                        .map(|(label, _)| label.to_string())
                        .collect();
                }
            }
            2 => {
                if let Some(outer) = &mut p.outer {
                    outer.op = match outer.op {
                        OuterOp::Quantile(_) => {
                            OuterOp::Quantile(QUANTILES[rng.below(QUANTILES.len())])
                        }
                        _ => *rng.pick(&[OuterOp::Sum, OuterOp::Count, OuterOp::Max]),
                    }
                }
            }
            _ => {
                if let Some(RangeFunction::QuantileOverTime(_)) = p.range {
                    p.range = Some(RangeFunction::QuantileOverTime(*rng.pick(&QUANTILES)));
                } else {
                    p.outer = None;
                }
            }
        },
        QuerySpec::Sql(s) => match rng.below(3) {
            0 => s.window_ms = *rng.pick(&WINDOWS_MS),
            1 => {
                s.group_by = SQL_COLUMNS
                    .iter()
                    .filter(|_| rng.chance(0.5))
                    .map(|(column, _)| column.to_string())
                    .collect();
                if let SqlAggregate::CountDistinct(column) = &s.aggregate {
                    s.group_by.retain(|c| c != column);
                }
            }
            _ => {
                if let SqlAggregate::Quantile(_) = s.aggregate {
                    s.aggregate = SqlAggregate::Quantile(*rng.pick(&QUANTILES));
                } else {
                    s.filters.clear();
                }
            }
        },
    }
    spec
}

/// A random query of `language`, a related edit of one of `existing` half
/// of the time.
pub fn random_query(rng: &mut Rng, language: Language, existing: &[FuzzQuery]) -> FuzzQuery {
    let related: Vec<&QuerySpec> = existing.iter().filter_map(|q| q.spec.as_ref()).collect();
    let spec = if !related.is_empty() && rng.chance(0.5) {
        let base = related[rng.below(related.len())].clone();
        related_spec(rng, &base)
    } else {
        match language {
            Language::Promql => QuerySpec::Promql(random_promql_spec(rng)),
            Language::Sql => QuerySpec::Sql(random_sql_spec(rng)),
        }
    };
    let ladder = accuracy_ladder();
    let accuracy = rng.pick(&ladder).clone();
    let repeat_interval_ms = *rng.pick(&REPEAT_INTERVALS_MS);
    let max_latency_ms = rng.chance(0.1).then(|| *rng.pick(&LATENCY_BOUNDS_MS));
    FuzzQuery::generated(spec, accuracy, repeat_interval_ms, max_latency_ms)
}

/// A random workload of 1 to 3 queries in one language over generated data
/// facts.
pub fn random_workload(rng: &mut Rng) -> FuzzWorkload {
    let language = if rng.chance(0.7) {
        Language::Promql
    } else {
        Language::Sql
    };
    let series_count = *rng.pick(&SERIES_COUNTS);
    let sample_interval_ms = *rng.pick(&SAMPLE_INTERVALS_MS);
    let mut queries = Vec::new();
    for _ in 0..1 + rng.below(3) {
        let query = random_query(rng, language, &queries);
        queries.push(query);
    }
    generated_workload(language, queries, series_count, sample_interval_ms)
}

/// A workload over the generator's sources (both PromQL metrics, or the SQL
/// table), each with `series_count` series sampled every
/// `sample_interval_ms`.
pub fn generated_workload(
    language: Language,
    queries: Vec<FuzzQuery>,
    series_count: u64,
    sample_interval_ms: u64,
) -> FuzzWorkload {
    let (sources, tables) = match language {
        Language::Promql => {
            let labels: Vec<(String, u64)> = PROMQL_LABELS
                .iter()
                .map(|(label, n)| (label.to_string(), *n))
                .collect();
            let sources = json!({
                GAUGE_METRIC: source_facts(series_count, sample_interval_ms, Some("gauge"), &labels),
                COUNTER_METRIC: source_facts(series_count, sample_interval_ms, Some("counter"), &labels),
            });
            (sources, Vec::new())
        }
        Language::Sql => {
            let labels: Vec<(String, u64)> = SQL_COLUMNS
                .iter()
                .map(|(column, n)| (column.to_string(), *n))
                .collect();
            let columns: Vec<&str> = SQL_COLUMNS.iter().map(|(column, _)| *column).collect();
            let sources = json!({
                SQL_TABLE: source_facts(series_count, sample_interval_ms, None, &labels),
            });
            (sources, vec![sql_table_declaration(SQL_TABLE, &columns)])
        }
    };
    let data = serde_json::from_value(json!({
        "arrival": {"ingesting": {"rows_per_second": 1.0}},
        "sources": sources,
    }))
    .expect("generated data facts deserialize");
    let mut workload = FuzzWorkload {
        language,
        queries,
        data,
        tables,
        raw_data_retained: true,
        query_time_results_retained: false,
    };
    workload.set_arrival_from_sources();
    workload
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed draws the same workloads.
    #[test]
    fn seeded_generation_is_reproducible() {
        let a: Vec<_> = (0..20)
            .map({
                let mut rng = Rng::new(7);
                move |_| random_workload(&mut rng)
            })
            .collect();
        let b: Vec<_> = (0..20)
            .map({
                let mut rng = Rng::new(7);
                move |_| random_workload(&mut rng)
            })
            .collect();
        assert_eq!(a, b);
    }

    /// Specs render to the PromQL and SQL the issues use.
    #[test]
    fn specs_render_promql_and_sql() {
        let promql = PromqlSpec {
            metric: "m_counter".into(),
            matchers: vec![("job".into(), "job1".into())],
            range: Some(RangeFunction::Rate),
            window_ms: 300_000,
            outer: Some(OuterAggregation {
                op: OuterOp::Sum,
                labels: vec!["job".into(), "instance".into()],
                without: false,
            }),
        };
        assert_eq!(
            promql.render(),
            "sum by (job, instance) (rate(m_counter{job=\"job1\"}[5m]))"
        );
        let sql = SqlSpec {
            table: "metrics".into(),
            aggregate: SqlAggregate::Quantile(0.9),
            group_by: vec!["city".into()],
            filters: vec![("city".into(), "NYC".into())],
            window_ms: 300_000,
        };
        assert_eq!(
            sql.render(),
            "SELECT city, percentile_cont(0.9) WITHIN GROUP (ORDER BY value) AS v FROM metrics \
             WHERE ts > now() - INTERVAL '5' MINUTE AND city = 'NYC' GROUP BY city"
        );
    }

    /// The ladder loosens and tightens one step at a time.
    #[test]
    fn accuracy_ladder_steps() {
        let ladder = accuracy_ladder();
        assert_eq!(looser(&ladder[0]), Some(ladder[1].clone()));
        assert_eq!(looser(&ladder[3]), None);
        assert_eq!(tighter(&ladder[1]), Some(ladder[0].clone()));
        assert_eq!(tighter(&ladder[0]), None);
    }
}
