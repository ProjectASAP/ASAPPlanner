// cargo run -p asap-devtools --bin planner_fuzz -- \
//     --seed 1 --cases 200 --time-budget-s 600 --out /tmp/planner-fuzz
// cargo run -p asap-devtools --bin planner_fuzz -- \
//     --seed 2 --cases 50 --relations a,b,tighter-accuracy --search candidate-connections \
//     --seed-workload eval/rangeheavy.workload.json --out out
//
// Metamorphic fuzz testing for missed reuse and sharing
// (`asap_devtools::planner_fuzz`; README "planner_fuzz"). Draws random
// small PromQL and SQL workloads from a seeded RNG (and 1–3 queries of a
// `--seed-workload` file a third of the time), checks every relation on
// each, shrinks each new violation, and writes under `--out`:
// `summary.json`, `summary.md` (counts per relation, the findings and the
// design docs' reuse patterns with their status) and `findings/`, one issue
// draft per distinct shrunk violation with its two workload files. Nothing
// is filed.
//
// `--relations` takes relation names or the letters a–o (default: all);
// `--search` is `candidate-connections` (the default search) or
// `exhaustive`; `--ignore-accuracy-guarantees` plans as `stage_pipeline`
// does by default (guarantees are enforced here by default, as
// `plan_installable` does). A relation is violated when it misses by more
// than max(`--relative-tolerance` (default 0.01) × cost,
// `--absolute-tolerance` (default 1e-6)). `--max-shrink-attempts` (default
// 60) bounds the checks one shrink runs. Each case and each shrink runs in
// a worker process (this binary with `--worker <request> <response>`),
// killed after `--case-timeout-s` (default 120; a shrink after twice that);
// `summary.md` lists the cases that timed out or crashed.

use std::path::PathBuf;

use asap_devtools::planner_fuzz::relations::Relation;
use asap_devtools::planner_fuzz::report::{run_campaign, serve, Campaign, WorkerRequest};
use asap_devtools::planner_fuzz::{read_seed_file, FuzzSettings, SearchKind};

const USAGE: &str = "usage: planner_fuzz --seed <u64> --cases <n> --time-budget-s <s> \
[--relations <name|letter>,...] [--search candidate-connections|exhaustive] \
[--seed-workload <file>]... [--ignore-accuracy-guarantees] [--relative-tolerance <f64>] \
[--absolute-tolerance <f64>] [--max-shrink-attempts <n>] [--case-timeout-s <s>] --out <dir>";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--worker") {
        // A campaign's worker process: one request in, one response out.
        let [_, request_path, response_path] = args.as_slice() else {
            eprintln!("planner_fuzz: --worker <request.json> <response.json>");
            std::process::exit(2);
        };
        let request: WorkerRequest = serde_json::from_str(
            &std::fs::read_to_string(request_path).expect("the request reads"),
        )
        .expect("the request parses");
        // One JSON line per response, flushed as it is ready, so a killed
        // worker still reports the checks it finished.
        let mut out = std::fs::File::create(response_path).expect("the response opens");
        serve(&request, &mut |response| {
            use std::io::Write;
            let line = serde_json::to_string(&response).expect("the response serializes");
            writeln!(out, "{line}").expect("the response writes");
            out.flush().expect("the response flushes");
        });
        return;
    }
    if let Err(message) = run(args) {
        eprintln!("planner_fuzz: {message}\n{USAGE}");
        std::process::exit(2);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    let mut seed = None;
    let mut cases = None;
    let mut time_budget_s = None;
    let mut relations: Vec<Relation> = Relation::ALL.to_vec();
    let mut settings = FuzzSettings::default();
    let mut seeds = Vec::new();
    let mut out = None;
    let mut max_shrink_attempts = 60;
    let mut case_timeout_s = 120.0;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--seed" => seed = Some(value()?.parse::<u64>().map_err(|e| e.to_string())?),
            "--cases" => cases = Some(value()?.parse::<usize>().map_err(|e| e.to_string())?),
            "--time-budget-s" => {
                time_budget_s = Some(value()?.parse::<f64>().map_err(|e| e.to_string())?)
            }
            "--relations" => {
                relations = value()?
                    .split(',')
                    .map(|name| Relation::parse(name).ok_or(format!("unknown relation {name}")))
                    .collect::<Result<_, _>>()?
            }
            "--search" => {
                settings.search = match value()?.as_str() {
                    "candidate-connections" => SearchKind::CandidateConnections,
                    "exhaustive" => SearchKind::Exhaustive,
                    other => return Err(format!("unknown search {other}")),
                }
            }
            "--seed-workload" => seeds.push(read_seed_file(&PathBuf::from(value()?))?),
            "--ignore-accuracy-guarantees" => settings.enforce_accuracy_guarantees = false,
            "--relative-tolerance" => {
                settings.relative_tolerance = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--absolute-tolerance" => {
                settings.absolute_tolerance = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--max-shrink-attempts" => {
                max_shrink_attempts = value()?.parse().map_err(|e| format!("{e}"))?
            }
            "--case-timeout-s" => case_timeout_s = value()?.parse().map_err(|e| format!("{e}"))?,
            "--out" => out = Some(PathBuf::from(value()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let campaign = Campaign {
        seed: seed.ok_or("--seed is required")?,
        cases: cases.ok_or("--cases is required")?,
        time_budget_s: time_budget_s.ok_or("--time-budget-s is required")?,
        relations,
        seeds,
        out_dir: out.ok_or("--out is required")?,
        max_shrink_attempts,
        worker: Some(std::env::current_exe().map_err(|e| e.to_string())?),
        case_timeout_s,
    };
    let summary = run_campaign(&campaign, settings).map_err(|e| e.to_string())?;
    println!(
        "{} cases, {} checks in {:.0} s; {} findings; summary in {}",
        summary.cases_run,
        summary.checks_run,
        summary.elapsed_s,
        summary.findings.len(),
        campaign.out_dir.join("summary.md").display()
    );
    for (relation, tally) in &summary.tallies {
        println!(
            "  {:<26} holds {:>5}  violated {:>4} ({} distinct)  inconclusive {:>5}  crashed {}",
            relation.name(),
            tally.holds,
            tally.violated,
            tally.distinct_violations.len(),
            tally.inconclusive,
            tally.crashed
        );
    }
    Ok(())
}
