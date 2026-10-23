//! The fuzzing campaign: draws workloads, checks every relation, shrinks
//! each new violation, and writes the summary and one issue draft per
//! distinct shrunk violation. Drafts are files only; nothing is filed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::generate::{random_query, random_workload, QuerySpec, Rng};
use super::relations::{
    evaluate, related_workloads, Check, Measurement, Outcome, Relation, Witness,
};
use super::shrink::shrink;
use super::{FuzzQuery, FuzzSettings, FuzzWorkload, Language, Planner};

/// One reuse pattern of the design docs and the relation that checks it.
pub struct DocumentedPattern {
    pub pattern: &'static str,
    /// The doc (under `docs/design_docs/`) section and line range on the
    /// main branch that states it.
    pub doc_reference: &'static str,
    pub relation: Option<Relation>,
    /// Why no relation checks it, or how one does.
    pub note: &'static str,
}

/// The reuse and sharing patterns the design docs say the planner
/// supports (or plans to), each with the relation that checks it.
pub fn documented_patterns() -> Vec<DocumentedPattern> {
    vec![
        DocumentedPattern {
            pattern: "Looser accuracy target reads the tighter build (max-sized shared summary; exact meets any ε)",
            doc_reference: "accuracy-models.md \"Sharing across consumers\" L308–368; planner-layering.md Pass 2 summary-capability rule L530–533, L640–641",
            relation: Some(Relation::LooserAccuracy),
            note: "",
        },
        DocumentedPattern {
            pattern: "Slice: a filter on a group key over a grouped result or summary",
            doc_reference: "asap-primitive-schema.md \"Other uses of coverage\" L846–848 (slice, planned), L118",
            relation: Some(Relation::FilterOnGroupKey),
            note: "documented as planned",
        },
        DocumentedPattern {
            pattern: "Rollup to a coarser grouping: exact sum/count/min/max, or a sketch SummaryMerge with group_by",
            doc_reference: "planner-layering.md Pass 2 rule table L588; asap-primitive-schema.md §5.6 Rollup L670, Example C L754–794, rollup guarantee table L830–844; joint-planning-search.md L324",
            relation: Some(Relation::CoarserGrouping),
            note: "",
        },
        DocumentedPattern {
            pattern: "Scan widening: two scans of one metric projecting different labels share one widened scan",
            doc_reference: "planner-layering.md Pass 2 L597–607",
            relation: Some(Relation::CoarserGrouping),
            note: "checked through the sketch rollup witness, which reads the finer query's scan and state",
        },
        DocumentedPattern {
            pattern: "Nested window: a shorter window merges the newest panes of a longer window's chain",
            doc_reference: "planner-layering.md window composition L790–807; asap-primitive-schema.md §5.6 \"Newest panes\" L669",
            relation: Some(Relation::NestedShorterWindow),
            note: "",
        },
        DocumentedPattern {
            pattern: "Another quantile rank from the same sketch",
            doc_reference: "planner-layering.md \"Other quantiles share for free\" L631–634, L804–817",
            relation: Some(Relation::OtherQuantileRank),
            note: "",
        },
        DocumentedPattern {
            pattern: "Identical expressions (common-subexpression elimination)",
            doc_reference: "planner-layering.md Pass 2 rule table L583–585, Algorithm 2 L655–658",
            relation: Some(Relation::ExactDuplicate),
            note: "",
        },
        DocumentedPattern {
            pattern: "A query over another query's result (topk over a shared sum) reads that result",
            doc_reference: "joint-planning-search.md worked example L177–187; planner-layering.md L583–585",
            relation: Some(Relation::OuterOverResult),
            note: "",
        },
        DocumentedPattern {
            pattern: "Summary capability: another statistic the family answers from one build (FamilyCapability::answers)",
            doc_reference: "planner-layering.md Pass 2 rule table L586, examples L611–617, Example 2 L1306–1370",
            relation: Some(Relation::OtherStatistic),
            note: "checks the Total (count) readout",
        },
        DocumentedPattern {
            pattern: "gcd pane grid across repeat intervals: queries at different intervals read one chain at their own cadence",
            doc_reference: "planner-layering.md L791–809",
            relation: Some(Relation::OtherRepeatInterval),
            note: "",
        },
        DocumentedPattern {
            pattern: "Exact top-k reuse: a smaller k reads the first rows of the larger k's sorted result",
            doc_reference: "planner-layering.md Pass 2 rule table L589; joint-planning-search.md L325",
            relation: Some(Relation::SmallerTopK),
            note: "",
        },
        DocumentedPattern {
            pattern: "Merge of disjoint selections: the union of two filtered states answers the union filter",
            doc_reference: "asap-primitive-schema.md §4.2 L194–232, §5.6 Example B L721–752",
            relation: Some(Relation::UnionOfSelections),
            note: "the merge operator is implemented; the docs name no Pass 2 rule that creates it",
        },
        DocumentedPattern {
            pattern: "Panes kept across evaluations when the deployment retains query-time results",
            doc_reference: "planner-layering.md Materialization L889–893",
            relation: Some(Relation::ResultsRetained),
            note: "a capability never raises cost",
        },
        DocumentedPattern {
            pattern: "Window = interval: the whole window maintained as a one-pane tumbling chain",
            doc_reference: "planner-layering.md Materialization L895–905",
            relation: None,
            note: "a Stage 2 choice within one query, not a q → q' derivation",
        },
        DocumentedPattern {
            pattern: "Rollup over newest panes (windowed rollup)",
            doc_reference: "planner-layering.md L809–823",
            relation: None,
            note: "a composition of c and d; not checked as one relation",
        },
        DocumentedPattern {
            pattern: "avg as sum/count: shared exact accumulators",
            doc_reference: "planner-layering.md L372–383, L502–503",
            relation: None,
            note: "the derivation needs two accumulators, not one operator over q",
        },
        DocumentedPattern {
            pattern: "Stored-state reuse (view matching); sliding windows, exponential histograms, SummarySubtract",
            doc_reference: "asap-primitive-schema.md L849, L851–869; planner-layering.md L618–630, L827",
            relation: None,
            note: "documented as planned or not implemented; no operator to derive with yet",
        },
    ]
}

/// The relations' counts over a campaign.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Tally {
    pub holds: usize,
    pub violated: usize,
    pub inconclusive: usize,
    /// Checks the planner panicked in (also listed, with their message,
    /// under `inconclusive_reasons`).
    pub crashed: usize,
    /// Distinct violation signatures ([`signature`]) before shrinking.
    pub distinct_violations: BTreeSet<String>,
    /// Each inconclusive reason (its first 400 characters), with counts.
    pub inconclusive_reasons: BTreeMap<String, usize>,
}

/// One shrunk violation with its issue draft.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: String,
    pub relation: Relation,
    /// The violation's class ([`draft_key`]): one draft per class.
    pub draft_key: String,
    /// The shrunk check's [`signature`].
    pub signature: String,
    pub case: usize,
    pub queries_before_shrinking: usize,
    /// Checks the shrink ran; `None` when the shrink failed or timed out
    /// and the draft shows the unshrunk check.
    pub shrink_attempts: Option<usize>,
    pub check: Check,
    pub measurement: Measurement,
    /// The open issue a reader should compare it with, when the relation
    /// matches one (a guess from the relation, not a proof).
    pub possibly_known_issue: Option<&'static str>,
    pub draft: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub seed: u64,
    pub settings: FuzzSettings,
    pub cases_run: usize,
    pub checks_run: usize,
    pub elapsed_s: f64,
    pub tallies: BTreeMap<Relation, Tally>,
    pub findings: Vec<Finding>,
    /// Cases whose worker timed out or crashed (e.g. a stack overflow).
    pub failed_cases: Vec<FailedCase>,
}

/// A case whose worker timed out or crashed; the checks it finished are
/// counted.
#[derive(Debug, Clone, Serialize)]
pub struct FailedCase {
    pub case: usize,
    pub error: String,
    pub queries: Vec<String>,
    /// The check that was running: its relation and target query.
    pub running_check: Option<(Relation, usize)>,
    /// The last lines the worker wrote to its standard error.
    pub stderr_tail: String,
}

/// What a campaign runs.
pub struct Campaign {
    pub seed: u64,
    pub cases: usize,
    pub time_budget_s: f64,
    pub relations: Vec<Relation>,
    /// Workloads read from seed files; a case draws 1–3 queries of one
    /// a third of the time.
    pub seeds: Vec<FuzzWorkload>,
    pub out_dir: PathBuf,
    pub max_shrink_attempts: usize,
    /// The executable that serves `--worker` requests (the `planner_fuzz`
    /// binary): each case and each shrink runs in its own process, killed
    /// after `case_timeout_s` (a shrink after twice that). `None` runs them
    /// in this process, unbounded.
    pub worker: Option<PathBuf>,
    pub case_timeout_s: f64,
}

/// The shape of one query without its constants (metrics, labels, values,
/// windows, ranks), plus whether its target is exact.
fn query_shape(q: &FuzzQuery) -> String {
    let exact = if q.accuracy().is_exact() {
        "exact"
    } else {
        "approx"
    };
    let kind = |debug: String| debug.split('(').next().unwrap_or_default().to_string();
    match &q.spec {
        Some(QuerySpec::Promql(s)) => format!(
            "{}({}{})[{exact}]",
            s.outer
                .as_ref()
                .map(|o| format!(
                    "{}{}{}",
                    kind(format!("{:?}", o.op)),
                    if o.without { " without" } else { " by" },
                    o.labels.len()
                ))
                .unwrap_or_default(),
            s.range
                .map(|r| kind(format!("{r:?}")))
                .unwrap_or_else(|| "instant".into()),
            if s.matchers.is_empty() {
                ""
            } else {
                " filtered"
            },
        ),
        Some(QuerySpec::Sql(s)) => format!(
            "sql {}(by {}{})[{exact}]",
            kind(format!("{:?}", s.aggregate)),
            s.group_by.len(),
            if s.filters.is_empty() {
                ""
            } else {
                " filtered"
            },
        ),
        None => format!(
            "{}[{exact}]",
            q.text()
                .chars()
                .filter(|c| !c.is_ascii_digit())
                .collect::<String>()
        ),
    }
}

/// A check's shape, for counting distinct violations: the relation, the
/// target query's shape and the other queries' shapes.
pub fn signature(check: &Check) -> String {
    let target = check
        .workload
        .queries
        .get(check.target)
        .map(query_shape)
        .unwrap_or_default();
    let mut others: Vec<String> = check
        .workload
        .queries
        .iter()
        .enumerate()
        .filter(|(i, _)| !check.relation.per_query() || *i != check.target)
        .map(|(_, q)| query_shape(q))
        .collect();
    others.sort();
    format!(
        "{} | q: {target} | others: {}",
        check.relation.name(),
        others.join(", ")
    )
}

/// A violation's class, for one draft per likely cause: the relation, the
/// language, how the witness derives `q'` and whether `q` is exact.
pub fn draft_key(check: &Check, measurement: &Measurement) -> String {
    let derivation = measurement
        .witness
        .as_ref()
        .and_then(|w| w.derivation.as_ref())
        .map(|d| {
            format!("{d:?}")
                .split([' ', '{', '('])
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_else(|| "none".into());
    let exact = check
        .workload
        .queries
        .get(check.target)
        .map(|q| {
            if q.accuracy().is_exact() {
                "exact"
            } else {
                "approx"
            }
        })
        .unwrap_or_default();
    format!(
        "{} | {:?} | {derivation} | q {exact}",
        check.relation.name(),
        check.workload.language
    )
}

/// The open issue a violation of `relation` most likely duplicates.
fn possibly_known_issue(relation: Relation, check: &Check) -> Option<&'static str> {
    let sql = check.workload.language == Language::Sql;
    match relation {
        Relation::LooserAccuracy => Some("#903"),
        Relation::FilterOnGroupKey if sql => Some("#902"),
        Relation::FilterOnGroupKey => Some("#891"),
        _ => None,
    }
}

fn evaluate_caught(planner: &mut Planner, check: &Check) -> Outcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| evaluate(planner, check)))
        .unwrap_or_else(|panic| Outcome::Inconclusive {
            reason: format!(
                "panic: {}",
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default()
            ),
        })
}

fn draw_case(rng: &mut Rng, seeds: &[FuzzWorkload]) -> FuzzWorkload {
    if !seeds.is_empty() && rng.chance(1.0 / 3.0) {
        let seed = &seeds[rng.below(seeds.len())];
        let mut workload = seed.clone();
        let count = (1 + rng.below(3)).min(seed.queries.len());
        let mut picked = BTreeSet::new();
        while picked.len() < count {
            picked.insert(rng.below(seed.queries.len()));
        }
        workload.queries = picked
            .into_iter()
            .map(|i| seed.queries[i].clone())
            .collect();
        workload
    } else {
        random_workload(rng)
    }
}

/// Work a campaign hands to a worker: one case's checks, or one shrink.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRequest {
    /// Check every relation of `relations` on `workload` (per query for a
    /// per-query relation), with `added` the query `AddedQuery` adds.
    Case {
        workload: FuzzWorkload,
        added: Option<FuzzQuery>,
        relations: Vec<Relation>,
        settings: FuzzSettings,
    },
    /// Shrink a violated check.
    Shrink {
        check: Check,
        settings: FuzzSettings,
        max_attempts: usize,
    },
}

/// One response line of a worker: a case answers one `Check` line per
/// check as it finishes, a shrink one `Shrink` line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerResponse {
    Check {
        check: Box<Check>,
        outcome: Box<Outcome>,
    },
    Shrink {
        check: Box<Check>,
        outcome: Box<Outcome>,
        attempts: usize,
    },
}

/// The checks of a case, in the order a worker runs them.
fn case_checks(
    workload: &FuzzWorkload,
    added: &Option<FuzzQuery>,
    relations: &[Relation],
) -> Vec<Check> {
    let mut checks = Vec::new();
    for &relation in relations {
        let targets = if relation.per_query() {
            0..workload.queries.len()
        } else {
            0..1
        };
        for target in targets {
            checks.push(Check {
                relation,
                workload: workload.clone(),
                target,
                added: added.clone(),
            });
        }
    }
    checks
}

/// Does `request` in this process, with a fresh plan cache, handing each
/// response line to `respond` as it is ready.
pub fn serve(request: &WorkerRequest, respond: &mut dyn FnMut(WorkerResponse)) {
    match request {
        WorkerRequest::Case {
            workload,
            added,
            relations,
            settings,
        } => {
            let mut planner = Planner::new(settings.clone());
            for check in case_checks(workload, added, relations) {
                let outcome = evaluate_caught(&mut planner, &check);
                respond(WorkerResponse::Check {
                    check: Box::new(check),
                    outcome: Box::new(outcome),
                });
            }
        }
        WorkerRequest::Shrink {
            check,
            settings,
            max_attempts,
        } => {
            let mut planner = Planner::new(settings.clone());
            let mut still =
                |c: &Check| matches!(evaluate_caught(&mut planner, c), Outcome::Violated(_));
            let (shrunk, attempts) = shrink(check, &mut still, *max_attempts);
            let outcome = evaluate_caught(&mut planner, &shrunk);
            respond(WorkerResponse::Shrink {
                check: Box::new(shrunk),
                outcome: Box::new(outcome),
                attempts,
            });
        }
    }
}

/// `request` done by a worker process (`worker` with `--worker <request>
/// <response>`, writing JSON lines), killed after `timeout_s`; in this
/// process without a worker. Returns the response lines written and, when
/// the worker failed, why and the tail of its standard error.
fn run_worker(
    campaign: &Campaign,
    request: &WorkerRequest,
    timeout_s: f64,
) -> (Vec<WorkerResponse>, Option<(String, String)>) {
    let mut responses = Vec::new();
    let Some(worker) = &campaign.worker else {
        serve(request, &mut |r| responses.push(r));
        return (responses, None);
    };
    let dir = campaign.out_dir.join("work");
    let request_path = dir.join("request.json");
    let response_path = dir.join("response.jsonl");
    let stderr_path = dir.join("stderr.txt");
    let started = (|| -> std::io::Result<std::process::Child> {
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::remove_file(&response_path);
        std::fs::write(&request_path, serde_json::to_string(request)?)?;
        std::process::Command::new(worker)
            .arg("--worker")
            .arg(&request_path)
            .arg(&response_path)
            .stderr(std::fs::File::create(&stderr_path)?)
            .spawn()
    })();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => return (responses, Some((format!("worker: {e}"), String::new()))),
    };
    let clock = Instant::now();
    let error = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break None,
            Ok(Some(status)) => break Some(format!("worker exited with {status}")),
            Err(e) => break Some(format!("worker: {e}")),
            Ok(None) if clock.elapsed().as_secs_f64() > timeout_s => {
                let _ = child.kill();
                let _ = child.wait();
                break Some(format!("timed out after {timeout_s} s"));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let text = std::fs::read_to_string(&response_path).unwrap_or_default();
    // A killed worker may leave a partial last line; it is dropped.
    responses.extend(
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok()),
    );
    let stderr_tail = || {
        let text = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(5)..].join("\n")
    };
    (responses, error.map(|e| (e, stderr_tail())))
}

/// Runs `campaign` and writes `summary.json`, `summary.md` and the drafts
/// under `findings/` of its output directory.
pub fn run_campaign(campaign: &Campaign, settings: FuzzSettings) -> std::io::Result<Summary> {
    let started = Instant::now();
    let findings_dir = campaign.out_dir.join("findings");
    std::fs::create_dir_all(&findings_dir)?;
    let mut rng = Rng::new(campaign.seed);
    let mut summary = Summary {
        seed: campaign.seed,
        settings: settings.clone(),
        cases_run: 0,
        checks_run: 0,
        elapsed_s: 0.0,
        tallies: campaign
            .relations
            .iter()
            .map(|r| (*r, Tally::default()))
            .collect(),
        findings: Vec::new(),
        failed_cases: Vec::new(),
    };
    let mut drafted = BTreeSet::new();
    for case in 0..campaign.cases {
        if started.elapsed().as_secs_f64() > campaign.time_budget_s {
            break;
        }
        let workload = draw_case(&mut rng, &campaign.seeds);
        let added = workload
            .all_structured()
            .then(|| random_query(&mut rng, workload.language, &workload.queries));
        summary.cases_run += 1;
        let request = WorkerRequest::Case {
            workload: workload.clone(),
            added: added.clone(),
            relations: campaign.relations.clone(),
            settings: settings.clone(),
        };
        let (responses, error) = run_worker(campaign, &request, campaign.case_timeout_s);
        if let Some((error, stderr_tail)) = error {
            let running = case_checks(&workload, &added, &campaign.relations)
                .get(responses.len())
                .map(|c| (c.relation, c.target));
            summary.failed_cases.push(FailedCase {
                case,
                error,
                queries: workload
                    .queries
                    .iter()
                    .map(|q| q.text().to_string())
                    .collect(),
                running_check: running,
                stderr_tail,
            });
        }
        for response in responses {
            let WorkerResponse::Check { check, outcome } = response else {
                continue;
            };
            summary.checks_run += 1;
            let relation = check.relation;
            let tally = summary.tallies.get_mut(&relation).expect("tallied");
            let measurement = match *outcome {
                Outcome::Holds(_) => {
                    tally.holds += 1;
                    continue;
                }
                Outcome::Inconclusive { reason } => {
                    if reason.starts_with("panic") {
                        tally.crashed += 1;
                    } else {
                        tally.inconclusive += 1;
                    }
                    let reason: String = reason.chars().take(400).collect();
                    *tally.inconclusive_reasons.entry(reason).or_default() += 1;
                    continue;
                }
                Outcome::Violated(measurement) => measurement,
            };
            tally.violated += 1;
            tally.distinct_violations.insert(signature(&check));
            // One draft per class: a class already drafted is counted, not
            // shrunk again.
            let original_key = draft_key(&check, &measurement);
            if !drafted.insert(original_key) {
                continue;
            }
            let request = WorkerRequest::Shrink {
                check: (*check).clone(),
                settings: settings.clone(),
                max_attempts: campaign.max_shrink_attempts,
            };
            let (responses, _) = run_worker(campaign, &request, 2.0 * campaign.case_timeout_s);
            let shrunk = responses.into_iter().find_map(|r| match r {
                WorkerResponse::Shrink {
                    check,
                    outcome,
                    attempts,
                } => match *outcome {
                    Outcome::Violated(m) => Some((*check, m, attempts)),
                    _ => None,
                },
                _ => None,
            });
            let (shrunk, shrunk_measurement, attempts) = match shrunk {
                Some((c, m, a)) => (c, m, Some(a)),
                None => ((*check).clone(), measurement, None),
            };
            let key = draft_key(&shrunk, &shrunk_measurement);
            // The shrunk check may land in another class; draft it once.
            if key != draft_key(&check, &shrunk_measurement) && !drafted.insert(key.clone()) {
                continue;
            }
            let mut finding = Finding {
                id: format!("s{}-{:03}", campaign.seed, summary.findings.len() + 1),
                relation,
                draft_key: key,
                signature: signature(&shrunk),
                case,
                queries_before_shrinking: check.workload.queries.len(),
                shrink_attempts: attempts,
                possibly_known_issue: possibly_known_issue(relation, &shrunk),
                check: shrunk,
                measurement: shrunk_measurement,
                draft: PathBuf::new(),
            };
            finding.draft = write_draft(&findings_dir, &finding, campaign.seed, &settings)?;
            summary.findings.push(finding);
        }
        // Keep the summary current, so an interrupted run leaves one.
        write_summary(campaign, &mut summary, &started)?;
    }
    write_summary(campaign, &mut summary, &started)?;
    Ok(summary)
}

fn write_summary(
    campaign: &Campaign,
    summary: &mut Summary,
    started: &Instant,
) -> std::io::Result<()> {
    summary.elapsed_s = started.elapsed().as_secs_f64();
    std::fs::write(
        campaign.out_dir.join("summary.json"),
        serde_json::to_string_pretty(&summary).map_err(std::io::Error::other)? + "\n",
    )?;
    std::fs::write(
        campaign.out_dir.join("summary.md"),
        summary_markdown(summary),
    )
}

fn derivation_text(witness: &Witness) -> String {
    match witness {
        Witness::Same => "reading q's result node itself".into(),
        Witness::Filter { column, value } => {
            format!("a `Filter({column} = '{value}')` over q's result")
        }
        Witness::Graft { text } => format!("`{text}` with its inner `q` read from q's result"),
        Witness::ExactRollup { labels, op } => format!(
            "an exact `{op:?}` grouped by ({}) over q's result",
            labels.join(", ")
        ),
        Witness::SmallerTopK { from, to } => {
            format!("q's top-{from} at {to} rows per group over the same input")
        }
        Witness::SketchRollup { labels } => format!(
            "a `SummaryMerge` with `group_by` = ({}) over q's summary state, then q's estimate",
            labels.join(", ")
        ),
        Witness::NewestPanes { window_ms } => format!(
            "a `SummaryMerge` of the newest panes of q's pane chain covering {window_ms} ms"
        ),
        Witness::QuantileRank { from, to } => {
            format!("q's quantile estimate at rank {to} instead of {from}, from the same state")
        }
        Witness::OtherStatistic { statistic } => {
            format!("a `SummaryEstimate` of {statistic:?} from q's summary state")
        }
        Witness::UnionWithLast => {
            "a `SummaryMerge` of q's state and the other selection's state, then q's estimate"
                .into()
        }
    }
}

fn write_draft(
    dir: &Path,
    finding: &Finding,
    seed: u64,
    settings: &FuzzSettings,
) -> std::io::Result<PathBuf> {
    let check = &finding.check;
    let m = &finding.measurement;
    let (base, related, _) = related_workloads(check)
        .unwrap_or_else(|_| (check.workload.clone(), check.workload.clone(), None));
    let workload_json = |w: &FuzzWorkload| {
        serde_json::to_string(&w.planning_workload()).expect("a workload serializes")
    };
    let workload_file = |w: &FuzzWorkload| {
        serde_json::to_string_pretty(&w.planning_workload()).expect("a workload serializes") + "\n"
    };
    std::fs::write(
        dir.join(format!("{}-base.workload.json", finding.id)),
        workload_file(&base),
    )?;
    std::fs::write(
        dir.join(format!("{}-related.workload.json", finding.id)),
        workload_file(&related),
    )?;
    let marginal = check.relation.is_marginal();
    let (base_name, related_name) = if marginal {
        ("W", "W ∪ {q'}")
    } else {
        ("W", "W'")
    };
    let q = check
        .workload
        .queries
        .get(check.target)
        .map(|q| q.text().to_string())
        .unwrap_or_default();
    let title = match check.relation {
        Relation::LooserAccuracy => {
            format!("`{q}` at a looser accuracy target does not read the tighter query's result")
        }
        Relation::FilterOnGroupKey => {
            format!("A query filtered on a group key does not read `{q}`'s grouped result")
        }
        Relation::CoarserGrouping => {
            format!("A coarser grouping of `{q}` does not roll up its result or state")
        }
        Relation::NestedShorterWindow => {
            format!("A shorter window of `{q}` does not merge the newest panes of its chain")
        }
        Relation::OtherQuantileRank => {
            format!("Another quantile rank of `{q}` does not read the same sketch")
        }
        Relation::ExactDuplicate => {
            format!("A duplicate of `{q}` is not answered from the same result")
        }
        Relation::OuterOverResult => {
            format!("`topk(3, …)` over `{q}` does not read `{q}`'s result")
        }
        Relation::OtherStatistic => {
            format!("A count over `{q}`'s input does not read the count `{q}`'s sketch answers")
        }
        Relation::OtherRepeatInterval => {
            format!("`{q}` at a longer repeat interval does not read the same result or state")
        }
        Relation::SmallerTopK => format!("A smaller k of `{q}` does not read its top-k result"),
        Relation::UnionOfSelections => {
            format!("A union of two selections of `{q}` does not merge their states")
        }
        Relation::TighterAccuracy => "Tightening an accuracy target lowers the planned cost".into(),
        Relation::AddedQuery => "Adding a query lowers the planned cost".into(),
        Relation::NoRawRetention => "Dropping raw-data retention lowers the planned cost".into(),
        Relation::ResultsRetained => "Retaining query-time results raises the planned cost".into(),
        Relation::RelaxedLatency => "Relaxing a latency bound raises the planned cost".into(),
        Relation::Renamed => "Renaming the metric and labels changes the planned cost".into(),
        Relation::Reordered => "Reordering the queries changes the planned cost".into(),
        Relation::DisjointCopy => {
            "Planning a disjoint copy of a workload does not double its cost".into()
        }
    };
    let mut text = format!("# {title}\n\n## Example\n\n");
    let shrunk = match finding.shrink_attempts {
        Some(attempts) => format!(
            "shrunk from {} to {} queries in {attempts} checks",
            finding.queries_before_shrinking,
            check.workload.queries.len()
        ),
        None => "not shrunk (the shrink failed or timed out)".into(),
    };
    text.push_str(&format!(
        "Found by `planner_fuzz` (seed {seed}, case {}, relation `{}`), {shrunk}. \
         Search: {:?}; accuracy guarantees {}.\n\n",
        finding.case,
        check.relation.name(),
        settings.search,
        if settings.enforce_accuracy_guarantees {
            "enforced"
        } else {
            "ignored"
        },
    ));
    text.push_str(&format!(
        "`{base_name}` (save as `base.json`):\n\n```json\n{}\n```\n\n",
        workload_json(&base)
    ));
    match &m.related_query {
        Some(query) if marginal => text.push_str(&format!(
            "`{related_name}` adds `q'` = `{query}` (save as `related.json`):\n\n"
        )),
        Some(query) => text.push_str(&format!(
            "`{related_name}` adds `{query}` (save as `related.json`):\n\n"
        )),
        None => text.push_str(&format!("`{related_name}` (save as `related.json`):\n\n")),
    }
    text.push_str(&format!("```json\n{}\n```\n\n", workload_json(&related)));
    text.push_str(&format!(
        "```sh\n{}\n{}\n```\n\n",
        base.stage_pipeline_command("base.json", settings),
        related.stage_pipeline_command("related.json", settings),
    ));
    text.push_str("| Workload | Selected plan | Cost per second |\n|---|---|---|\n");
    text.push_str(&format!(
        "| `{base_name}` | `{}` | {:.6} |\n",
        m.base_plan, m.base_cost_per_second
    ));
    text.push_str(&format!(
        "| `{related_name}` | `{}` | {:.6} |\n",
        m.related_plan, m.related_cost_per_second
    ));
    if let Some(w) = &m.witness {
        let label = match &w.derivation {
            Some(d) => format!(
                "Witness: `W`'s plan with `q'` derived by {}",
                derivation_text(d)
            ),
            None => "The other side's plan, priced for this workload".into(),
        };
        text.push_str(&format!(
            "| {label} | `{}` | {:.6} |\n",
            w.plan, w.cost_per_second
        ));
    }
    text.push_str(&format!(
        "\n**Relation violated:** `{}`.\n\n{}\n\n",
        check.relation.statement(),
        m.summary
    ));
    let witness = m.witness.as_ref();
    match witness.and_then(|w| w.derive_cost_per_second.map(|d| (w, d))) {
        Some((w, derive_cost)) => text.push_str(&format!(
            "**Expected:** a candidate for `{related_name}` costing at most about {:.6} per second \
             (the witness): `W`'s plan ({:.6} as Stage 3 prices it) plus `q'` derived for \
             {derive_cost:.6}. Of that, the nodes only `q'` reads cost {:.6} ({}).\n\n",
            w.cost_per_second,
            w.base_reference_cost_per_second.unwrap_or_default(),
            w.derived_node_cost_per_second.unwrap_or_default(),
            if w.derived_operators.is_empty() {
                "none: `q'` reads `q`'s node".to_string()
            } else {
                w.derived_operators.join(", ")
            },
        )),
        None => text.push_str(&format!(
            "**Expected:** the selected costs satisfy `{}`.{}\n\n",
            check.relation.statement(),
            witness
                .map(|w| format!(
                    " The other side's plan prices at {:.6} per second for this workload.",
                    w.cost_per_second
                ))
                .unwrap_or_default()
        )),
    }
    text.push_str("## Why it happens\n\nNot diagnosed yet. ");
    if marginal {
        text.push_str(
            "The witness is priced by Stage 2 and Stage 3 (`optimize_physical_asap`, \
             `select_plan`) with the same models as the search, so the cheaper plan passes \
             Stage 3's checks, but the search did not select it.",
        );
        if witness.is_some_and(|w| w.key_filter_keeps_input_guarantee) {
            text.push_str(
                " Note: the witness's `Filter` on a group key keeps its input's accuracy \
                 guarantee, which Stage 1's guarantee composition does not give a filter over \
                 approximate rows (#689); the gap may be there as much as in the missing \
                 candidate rule.",
            );
        }
    } else {
        text.push_str(
            "The search's selections for the two workloads contradict the relation: one of \
             the two searches missed a cheaper plan.",
        );
        if witness.is_some() {
            text.push_str(
                " The other side's logical plan, priced for this workload by Stage 2 and \
                 Stage 3, is that plan.",
            );
        }
    }
    text.push_str("\n\n");
    text.push_str(&format!(
        "## Scope and acceptance\n\nA regression test that plans both workloads and asserts `{}`. \
         Today it fails with the numbers above.\n\n",
        check.relation.statement()
    ));
    text.push_str("## Status and related work\n\n");
    if let Some(issue) = finding.possibly_known_issue {
        text.push_str(&format!(
            "Compare with {issue} (same relation; check whether this is the same case).\n"
        ));
    } else {
        text.push_str("No open issue matched this relation automatically.\n");
    }
    let path = dir.join(format!("{}-{}.md", finding.id, check.relation.name()));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// The summary as Markdown: the counts per relation with their commonest
/// inconclusive reasons, the findings, the failed cases and panics, and the
/// design docs' reuse patterns with each relation's status in this run.
pub fn summary_markdown(summary: &Summary) -> String {
    let mut text = format!(
        "# planner_fuzz summary\n\nSeed {}, {} cases, {} checks, {:.0} s. Search {:?}, accuracy guarantees {}.\n\n",
        summary.seed,
        summary.cases_run,
        summary.checks_run,
        summary.elapsed_s,
        summary.settings.search,
        if summary.settings.enforce_accuracy_guarantees {
            "enforced"
        } else {
            "ignored"
        },
    );
    text.push_str("| Relation | Holds | Violated | Distinct violations | Inconclusive | Crashed |\n|---|---|---|---|---|---|\n");
    for (relation, tally) in &summary.tallies {
        text.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            relation.name(),
            tally.holds,
            tally.violated,
            tally.distinct_violations.len(),
            tally.inconclusive,
            tally.crashed
        ));
    }
    text.push_str("\n## Findings (one draft per class)\n\n");
    for finding in &summary.findings {
        text.push_str(&format!(
            "- `{}` `{}`: {} → {} (compare {})\n",
            finding.id,
            finding.draft_key,
            finding.signature,
            finding.draft.display(),
            finding.possibly_known_issue.unwrap_or("none")
        ));
    }
    if !summary.failed_cases.is_empty() {
        text.push_str(&format!(
            "\n## Cases that timed out or crashed ({})\n\n",
            summary.failed_cases.len()
        ));
        for failed in &summary.failed_cases {
            text.push_str(&format!(
                "- case {}: {} while checking {} — `{}`\n",
                failed.case,
                failed.error,
                failed
                    .running_check
                    .map(|(r, t)| format!("`{}` at q{}", r.name(), t + 1))
                    .unwrap_or_else(|| "nothing".into()),
                failed.queries.join("`, `")
            ));
        }
    }
    text.push_str("\n## Commonest inconclusive reasons\n\n");
    for (relation, tally) in &summary.tallies {
        let mut reasons: Vec<_> = tally.inconclusive_reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, count) in reasons.into_iter().take(3) {
            let reason: String = reason.chars().take(160).collect();
            text.push_str(&format!("- `{}` ({count}): {reason}\n", relation.name()));
        }
    }
    text.push_str("\n## Design-doc reuse patterns\n\n");
    text.push_str(&coverage_table(summary));
    text
}

/// The design docs' reuse patterns, the relation checking each, and its
/// status in `summary`.
pub fn coverage_table(summary: &Summary) -> String {
    let mut text =
        "| Pattern | Doc reference | Relation | Status in this run |\n|---|---|---|---|\n"
            .to_string();
    for p in documented_patterns() {
        let status = match p
            .relation
            .and_then(|r| summary.tallies.get(&r).map(|t| (r, t)))
        {
            None if p.relation.is_none() => format!("not checked: {}", p.note),
            None => "not run".into(),
            Some((r, t)) => {
                let findings: Vec<String> = summary
                    .findings
                    .iter()
                    .filter(|f| f.relation == r)
                    .map(|f| {
                        format!(
                            "{}{}",
                            f.id,
                            f.possibly_known_issue
                                .map(|i| format!(" (compare {i})"))
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                let conclusive = t.holds + t.violated;
                if t.violated > 0 {
                    format!(
                        "VIOLATED in {} of {conclusive} conclusive checks ({} distinct): {}",
                        t.violated,
                        t.distinct_violations.len(),
                        findings.join(", ")
                    )
                } else if t.holds > 0 {
                    format!("holds ({conclusive} conclusive checks)")
                } else {
                    format!("inconclusive ({} checks)", t.inconclusive)
                }
            }
        };
        text.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            p.pattern,
            p.doc_reference,
            p.relation
                .map(|r| format!("`{}`", r.name()))
                .unwrap_or_else(|| "—".into()),
            if p.note.is_empty() || p.relation.is_none() {
                status
            } else {
                format!("{status} ({})", p.note)
            }
        ));
    }
    text
}
