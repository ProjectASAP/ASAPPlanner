# `QueryExpr` type inventory

Reference tree of `QueryExpr` (`crates/types/src/intent_algebra/query_expr.rs`) and every type
it depends on. Generated from the current source — regenerate by hand if the enums drift.

## `QueryExpr` (query_expr.rs)

- `Scan { source: Source, predicates: Vec<Predicate>, schema: Schema }`
- `Ref { name: BindingName }`
- `Scalar(f64)`
- `EvalTime`
- `VectorFromScalar(Box<QueryExpr>)`
- `ScalarFromVector(Box<QueryExpr>)`
- `Relabel { dst: String, value: L3Expr, child }`
- `InfoJoin { selector: Vec<InfoMatcher>, child }`
- `Sample { by: GroupKeys, kind: SampleKind, child }`
- `Filter { pred: Predicate, child }`
- `Project { cols: Vec<ProjectItem>, qualifier: Option<String>, child }`
- `Aggregate { reduction: Reduction, aggs: Vec<AggIntent>, output_names: Vec<String>, having: Option<Predicate>, child }`
- `Window { kind: WindowKind, size: Duration, slide: Option<Duration>, child }`
- `Distinct { cols: Vec<ColumnId>, child }`
- `Merge { children: Vec<QueryExpr> }`
- `Join { kind: JoinKind, pred: Predicate, left, right }`
- `SetOp { kind: SetOpKind, all: bool, left, right }`
- `Sort { keys: Vec<SortKey>, partition_by: GroupKeys, child }`
- `Limit { n: usize, offset: usize, child }`
- `LetBinding { name: BindingName, expr, child }`
- `Subquery { range: Duration, resolution: Option<Duration>, child }`
- `TimeRange { range: Duration, child }`
- `TimeShift { shift: TimeShift, child }`
- `WindowFunc { func: WindowFuncKind, args: Vec<L3Expr>, partition_by: GroupKeys, order_by: Vec<SortKey>, output_name: String, child }`
- `BinaryOp { op: BinaryOpKind, lhs, rhs, vector_match: Option<VectorMatch> }`

## Supporting types (query_expr.rs)

- `GroupKeys` — positional `by`/`without` grouping keys, shared by `Aggregate.reduction`, `Sort.partition_by`, `WindowFunc.partition_by`, `Sample.by`
- `Reduction` — `Reduce(GroupKeys) | PerEntity`
- `WindowKind` — `Tumbling | Sliding | Session`
- `DataModel` — `TimeSeries | Tabular | Any`
- `Source` — `TimeSeries { metric } | Table { table_ref }`
- `BinaryOpKind` — `Arith(ArithOp) | Compare(CompareOp) | And | Or | Unless | Pow | Atan2`
- `JoinKind` — `Inner | Left | Right | Full | Cross | Semi | Anti`
- `SetOpKind` — `Union | Intersect | Except`
- `WindowFuncKind` — `RowNumber | Rank | DenseRank | Lag | Lead | FirstValue | LastValue | NthValue(Option<u64>) | Sum | Avg | Count | Min | Max`
- `InfoMatcher { label: String, op: CompareOp, value: String }`
- `SampleKind` — `LimitK(usize) | LimitRatio(f64)`
- `SortKey { expr: L3Expr, ascending: bool, nulls_first: bool }`
- `VectorMatch { kind: VectorMatchKind, labels: Vec<String>, grouping: Option<VectorGrouping> }`
- `VectorMatchKind` — `On | Ignoring`
- `VectorGrouping { side: GroupSide, labels: Vec<String> }`
- `GroupSide` — `Left | Right`
- `AtModifier` — `Start | End | Timestamp(i64)`
- `TimeShift { offset_ms: i64, at: Option<AtModifier> }` — the modifier struct carried by `QueryExpr::TimeShift.shift`; distinct from the `QueryExpr::TimeShift` node itself
- `Predicate(L3Expr)`
- `ProjectItem { alias: Option<String>, expr: L3Expr }`

## `AggIntent` (agg_intent.rs)

"What to compute" vocabulary for `Aggregate.aggs`.

- `Count { accuracy: AccuracyTarget }`
- `Sum { col: Option<ColumnId> }`
- `Min { col: Option<ColumnId> }`
- `Max { col: Option<ColumnId> }`
- `Avg { col: Option<ColumnId> }`
- `StdDev { col: Option<ColumnId>, population: bool }`
- `Variance { col: Option<ColumnId>, population: bool }`
- `Quantile { col: Option<ColumnId>, q: f64, accuracy: AccuracyTarget }`
- `TopK { k: usize, accuracy: AccuracyTarget }`
- `Cardinality { col: Option<ColumnId>, accuracy: AccuracyTarget }`
- `Rate`
- `Increase`
- `Changes`
- `Delta`
- `IDelta`
- `Deriv`
- `Resets`
- `PredictLinear { seconds: f64 }`
- `DoubleExpSmoothing { smoothing: f64, trend: f64 }`
- `HistogramCount`
- `HistogramSum`
- `HistogramAvg`
- `HistogramStdDev`
- `HistogramStdVar`
- `HistogramFraction { lower: f64, upper: f64 }`
- `HistogramQuantile { q: f64 }`
- `Math(MathFunc)`
- `Absent`
- `AbsentOverTime`
- `PresentOverTime`
- `TimeFn(TimeFunc)`
- `Group`
- `CountValues { label: String }`
- `LastOverTime`
- `FirstOverTime`
- `MadOverTime`
- `TsOfMinOverTime`
- `TsOfMaxOverTime`
- `TsOfFirstOverTime`
- `TsOfLastOverTime`
- `Extension { ext_kind: String, payload: serde_json::Value }`

Supporting types:

- `TimeFunc` — `Timestamp | Minute | Hour | DayOfWeek | DayOfMonth | DayOfYear | Month | Year | DaysInMonth`
- `MathFunc` — `Abs | Ceil | Floor | Exp | Ln | Log2 | Log10 | Sqrt | Sgn | Sin | Cos | Tan | Asin | Acos | Atan | Sinh | Cosh | Tanh | Asinh | Acosh | Atanh | Deg | Rad | Round { to_nearest } | Clamp { min, max } | ClampMin { min } | ClampMax { max }`
- `RankingMeasure` — `Frequency | WeightedSum | NonAdditive` (classifies a top-k ranking's heavy-hitter sketchability; not carried on any node, computed from an `AggIntent` by `ranking_measure`)

## `L3Expr` (expr_ir.rs)

`L3Expr = Expr<ColumnId>` — the positional scalar expression IR used inside `Predicate`,
`Relabel.value`, `SortKey.expr`, `WindowFunc.args`, `ProjectItem.expr`.

- `Expr<C>` — `Column(C) | Literal(L3Scalar) | Compare { left, op, right } | BoolAnd(Vec<Expr<C>>) | BoolOr(Vec<Expr<C>>) | Not | IsNull | IsNotNull | Cast { expr, to, try_cast } | InList { expr, list, negated } | FunctionCall { name, args } | Arith { op, left, right } | Case { operand, branches, else_expr }`
- `L3Scalar` — `Int64(i64) | Float64(f64) | Utf8(String) | Boolean(bool) | Null`
- `CompareOp` — `Eq | Ne | Lt | Le | Gt | Ge | Like | NotLike | ILike | NotILike | Regex | NotRegex`
- `ArithOp` — `Add | Sub | Mul | Div | Mod`
- `ColumnRef` — `Named(String) | Qualified { table, name } | SampleValue | Wildcard` — L2 only (name-based); the converter resolves every `ColumnRef` into a positional `ColumnId`, so it never appears inside `QueryExpr` itself, only in the pre-Binder `L2Expr = Expr<ColumnRef>`

## `Schema` (schema.rs)

Carried on `QueryExpr::Scan.schema`; derived for every other node's output by `output_schema_in`.

- `Schema { columns: Vec<Column>, time_index: Option<ColumnId>, unique_keys: Vec<Vec<ColumnId>>, closed: bool }`
- `Column { name: String, dtype: DataType, nullable: bool, table: Option<String> }`
- `DataType` — `Int64 | Float64 | Utf8 | Bool | Timestamp`
- `ColumnId = usize` — positional column index, not a named type

## `BindingName` (names.rs)

- `BindingName(pub String)` — identifier for `LetBinding.name` / `Ref.name`

## `AccuracyTarget` (types.rs)

Carried by the approximate `AggIntent` variants (`Count`, `Quantile`, `TopK`, `Cardinality`).

- `Epsilon(f64)`
- `EpsilonDelta { epsilon: f64, delta: f64 }`
- `Exact`
