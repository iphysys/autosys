//! `cargo xtask eval`: the dev benchmark run repeatedly against a running
//! api-server, scored, and written as the baseline (blueprint section 16.4,
//! ADR-0035). It orchestrates only: the request, the drive and the scores
//! are product code (blueprint section 10.1).
//!
//! The worker behind the server picks the model (`SYNTH_MODEL_PROFILE`);
//! the baseline records the model ids each report names.

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use api_client::drive::{DriveSettings, Driven, drive};
use api_client::{Client, ClientConfig, IdempotencyKey, Token};
use contracts::SchemaSet;
use contracts::types::{
    AgentSpec, ApprovalAnswer, Baseline, BaselineApprovalAnswer, BaselineProblem, BaselineRun,
    BaselineSchemaVersion, BenchmarkProblem, NonEmptyString, Timestamp, Usage,
};
use evaluation::baseline::{problem_sha256, summarize};
use evaluation::benchmarks::run_request;
use evaluation::outcomes::{RunRecord, all_passed, score, undecided};
use model_gateway::{ModelUsage, Routing};

use crate::{environment, problem_files};

const SCHEMA: &str = "reports/baseline.schema.json";
const DEFAULT_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_AGENT: &str = "main_orchestrator";
/// Time beyond a run's own wall-time budget before the eval cancels it.
const DEADLINE_MARGIN: Duration = Duration::from_secs(300);

/// The command line of `eval`.
#[derive(Debug, PartialEq)]
pub(crate) struct Args {
    /// Empty means every dev problem.
    pub problems: Vec<String>,
    pub repeat: NonZeroU64,
    pub max_cost_usd: f64,
    pub agent: String,
    pub out: Option<PathBuf>,
    /// Overrides each run's deadline.
    pub deadline: Option<Duration>,
}

pub(crate) fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let mut parsed = Args {
        problems: Vec::new(),
        repeat: NonZeroU64::new(5).context("5 is not zero")?,
        max_cost_usd: 5.0,
        agent: DEFAULT_AGENT.to_owned(),
        out: None,
        deadline: None,
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--problem" => parsed.problems.push(value("--problem")?),
            "--repeat" => {
                parsed.repeat = value("--repeat")?
                    .parse()
                    .context("--repeat takes a whole number of at least 1")?;
            }
            "--max-cost-usd" => {
                let cap: f64 = value("--max-cost-usd")?
                    .parse()
                    .context("--max-cost-usd takes a number")?;
                if !(cap.is_finite() && cap > 0.0) {
                    bail!("--max-cost-usd must be more than 0");
                }
                parsed.max_cost_usd = cap;
            }
            "--agent" => parsed.agent = value("--agent")?,
            "--out" => parsed.out = Some(PathBuf::from(value("--out")?)),
            "--deadline-seconds" => {
                let seconds: u64 = value("--deadline-seconds")?
                    .parse()
                    .context("--deadline-seconds takes a whole number")?;
                if seconds == 0 {
                    bail!("--deadline-seconds must be at least 1");
                }
                parsed.deadline = Some(Duration::from_secs(seconds));
            }
            other => bail!("unknown eval option `{other}`"),
        }
    }
    Ok(parsed)
}

pub(crate) fn run(root: &Path, args: Args) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the async runtime")?
        .block_on(eval(root, args))
}

/// The cost of one run: exact when it used one priced model, and the bound
/// the cost cap counts, at the dearest price of the models it used.
#[derive(Debug, PartialEq)]
struct Cost {
    exact: Option<f64>,
    bound: f64,
}

fn cost_of(routing: &Routing, model_ids: &[NonEmptyString], usage: &Usage) -> Cost {
    let at = |model: &str| {
        model_gateway::cost_usd(
            routing,
            &[ModelUsage {
                model: model.to_owned(),
                usage: contracts::types::TokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                },
            }],
        )
    };
    let costs: Vec<Option<f64>> = model_ids.iter().map(|m| at(m.as_str())).collect();
    let exact = match costs.as_slice() {
        [single] => *single,
        _ => None,
    };
    // A model without a price counts as nothing toward the cap; the warning
    // below says so.
    let bound = costs.iter().flatten().copied().fold(0.0, f64::max);
    Cost { exact, bound }
}

async fn eval(root: &Path, args: Args) -> Result<()> {
    let url = std::env::var("SYNTH_API_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned());
    let token = std::env::var("SYNTH_API_TOKEN")
        .context("SYNTH_API_TOKEN is not set: eval drives runs through api-server, which needs the development token")?;
    let client = Client::new(
        &url,
        Token::new(token).context("SYNTH_API_TOKEN is empty")?,
        ClientConfig::default(),
    )
    .context("building the API client")?;

    let config =
        infra::config::ConfigLoader::new(environment()?).context("loading the config schemas")?;
    let agent: AgentSpec = config
        .load(
            &root
                .join("config/agents")
                .join(format!("{}.yaml", args.agent)),
        )
        .with_context(|| format!("loading agent {}", args.agent))?;
    let routing_config: contracts::types::ModelRouting = config
        .load(&root.join("config/models/routing.yaml"))
        .context("loading the model routing")?;
    let profile = std::env::var(model_gateway::PROFILE_OVERRIDE_ENV)
        .unwrap_or_else(|_| routing_config.default_profile.to_string());
    let routing = Routing::new(
        routing_config,
        Arc::new(model_gateway::providers::default_registry().context("the model providers")?),
        config.environment(),
    )
    .context("checking the model routing")?;
    let problems = load_problems(root, &args.problems)?;

    let started = chrono::Utc::now();
    let out = args.out.clone().unwrap_or_else(|| {
        root.join("evals/baselines")
            .join(format!("{}-{profile}", started.format("%Y-%m-%d")))
    });
    let file = out.join("baseline.json");
    if file.exists() {
        bail!(
            "{} exists; pass --out for another directory",
            file.display()
        );
    }
    eprintln!(
        "xtask: eval: {} problem(s) x {} run(s) against {url}, at most ${:.2}",
        problems.len(),
        args.repeat,
        args.max_cost_usd
    );

    let mut spent = 0.0;
    let mut truncated = false;
    // A run that could not start ends the eval, but what was scored before
    // it is still written.
    let mut stopped: Option<anyhow::Error> = None;
    let mut model_ids: Vec<NonEmptyString> = Vec::new();
    let mut scored: Vec<BaselineProblem> = Vec::new();
    'problems: for problem in &problems {
        let request = run_request(problem, &agent)
            .with_context(|| format!("the request of {}", problem.id.as_str()))?;
        let deadline = args.deadline.unwrap_or_else(|| {
            let budget = request
                .budget
                .as_ref()
                .map_or(0, |b| b.max_wall_time_seconds.get());
            Duration::from_secs(budget) + DEADLINE_MARGIN
        });
        let settings = DriveSettings {
            answer: ApprovalAnswer::Approve,
            comment: NonEmptyString::try_from("Approved by cargo xtask eval.".to_owned()).ok(),
            deadline,
            grace: Duration::from_secs(120),
        };
        let mut runs: Vec<BaselineRun> = Vec::new();
        for n in 1..=args.repeat.get() {
            if spent >= args.max_cost_usd {
                truncated = true;
                eprintln!("xtask: eval: the cost cap is reached; no further runs start");
                scored.push(summary_of(problem, runs)?);
                break 'problems;
            }
            let key = IdempotencyKey::new(&format!(
                "eval-{}-{}-{n}",
                problem.id.as_str(),
                started.timestamp_millis()
            ))
            .context("an idempotency key")?;
            let run = match drive(&client, &request, &key, &settings).await {
                Ok(driven) => {
                    let (run, cost) = scored_run(problem, &driven, &routing)?;
                    spent += cost.bound;
                    if cost.exact.is_none() {
                        eprintln!(
                            "xtask: eval: warning: run {} has no exact cost ({:?})",
                            run.run_id.as_str(),
                            run.model_ids
                        );
                    }
                    run
                }
                Err(failure) => {
                    let Some(run_id) = failure.run_id else {
                        eprintln!("xtask: eval: a run could not start: {}", failure.error);
                        stopped = Some(anyhow::Error::new(failure.error).context("starting a run"));
                        truncated = true;
                        scored.push(summary_of(problem, runs)?);
                        break 'problems;
                    };
                    eprintln!(
                        "xtask: eval: warning: run {} could not be scored; its cost is not counted: {}",
                        run_id.as_str(),
                        failure.error
                    );
                    let reason = format!("the drive failed: {}", failure.error);
                    BaselineRun {
                        run_id,
                        stop_reason: None,
                        error: NonEmptyString::try_from(reason.clone()).ok(),
                        outcomes: undecided(problem, &reason)?,
                        usage: None,
                        cost_usd: None,
                        model_ids: Vec::new(),
                    }
                }
            };
            for id in &run.model_ids {
                if !model_ids.contains(id) {
                    model_ids.push(id.clone());
                }
            }
            eprintln!(
                "xtask: eval: {} run {n}/{}: {}, {}, ${:.4} so far",
                problem.id.as_str(),
                args.repeat,
                run.stop_reason
                    .map_or_else(|| "no report".to_owned(), |s| s.to_string()),
                if all_passed(&run.outcomes) {
                    "every outcome passes"
                } else {
                    "not every outcome passes"
                },
                spent
            );
            runs.push(run);
        }
        scored.push(summary_of(problem, runs)?);
    }

    let baseline = Baseline {
        schema_version: BaselineSchemaVersion::X01,
        created_at: Timestamp(started),
        git_commit: git_commit(root)?,
        agent_id: agent.id.clone(),
        repeat: args.repeat,
        max_cost_usd: args.max_cost_usd,
        approval_answer: BaselineApprovalAnswer::Approve,
        truncated,
        model_ids,
        problems: scored,
    };
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let text = serde_json::to_string_pretty(&baseline)? + "\n";
    let schemas = SchemaSet::compile(&[SCHEMA]).context("the baseline schema")?;
    let problems = schemas.errors_of(SCHEMA, &baseline)?;
    if !problems.is_empty() {
        // The runs cost money: keep them for inspection, never as a baseline.
        let kept = out.join("baseline.invalid.json");
        std::fs::write(&kept, text).with_context(|| format!("writing {}", kept.display()))?;
        bail!(
            "the baseline breaks its schema, so it was written to {} instead: {}",
            kept.display(),
            problems.join("; ")
        );
    }
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    for problem in &baseline.problems {
        let s = &problem.summary;
        eprintln!(
            "xtask: eval: {}: {}/{} runs passed every outcome",
            problem.problem_id.as_str(),
            s.all_outcomes_passed,
            s.runs
        );
    }
    eprintln!(
        "xtask: eval: ${spent:.4} spent; wrote {}{}",
        file.display(),
        if truncated { " (truncated)" } else { "" }
    );
    match stopped {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The dev problems `wanted` names, or all of them, in id order.
fn load_problems(root: &Path, wanted: &[String]) -> Result<Vec<BenchmarkProblem>> {
    let loader =
        infra::benchmarks::BenchmarkLoader::new().context("loading the benchmark schema")?;
    let dev = root.join("evals/benchmarks/dev");
    let mut problems = Vec::new();
    for path in problem_files(&root.join("evals/benchmarks"))? {
        if !path.starts_with(&dev) {
            continue;
        }
        let problem = loader
            .load(&path)
            .with_context(|| format!("loading {}", path.display()))?;
        if wanted.is_empty() || wanted.iter().any(|w| w == problem.id.as_str()) {
            problems.push(problem);
        }
    }
    for id in wanted {
        if !problems.iter().any(|p| p.id.as_str() == id) {
            bail!("no dev problem {id} in evals/benchmarks/dev/");
        }
    }
    if problems.is_empty() {
        bail!("evals/benchmarks/dev/ has no problems");
    }
    Ok(problems)
}

/// The scored run, and its cost.
fn scored_run(
    problem: &BenchmarkProblem,
    driven: &Driven,
    routing: &Routing,
) -> Result<(BaselineRun, Cost)> {
    let report = &driven.report;
    let outcomes = score(
        problem,
        &RunRecord {
            report,
            events: &driven.events,
            versions: &driven.versions,
        },
    )?;
    let usage = total_usage(report);
    let model_ids = report.versions.model_ids.clone();
    let cost = cost_of(routing, &model_ids, &usage);
    let error = driven.cancelled.then(|| {
        NonEmptyString::try_from("the eval cancelled the run at its deadline".to_owned()).ok()
    });
    Ok((
        BaselineRun {
            run_id: driven.run.run_id.clone(),
            stop_reason: Some(report.stop_reason),
            error: error.flatten(),
            outcomes,
            usage: Some(usage),
            cost_usd: cost.exact,
            model_ids,
        },
        cost,
    ))
}

/// The usage of every agent of the run, added up.
fn total_usage(report: &contracts::types::RunReport) -> Usage {
    report.budget_used.iter().fold(
        Usage {
            input_tokens: 0,
            output_tokens: 0,
            tool_calls: 0,
            steps: 0,
            wall_time_seconds: 0.0,
        },
        |total, agent| Usage {
            input_tokens: total.input_tokens + agent.usage.input_tokens,
            output_tokens: total.output_tokens + agent.usage.output_tokens,
            tool_calls: total.tool_calls + agent.usage.tool_calls,
            steps: total.steps + agent.usage.steps,
            wall_time_seconds: total.wall_time_seconds + agent.usage.wall_time_seconds,
        },
    )
}

fn summary_of(problem: &BenchmarkProblem, runs: Vec<BaselineRun>) -> Result<BaselineProblem> {
    Ok(BaselineProblem {
        problem_id: problem.id.clone(),
        problem_sha256: problem_sha256(problem)?,
        summary: summarize(problem, &runs),
        runs,
    })
}

/// `git describe --always --dirty`, or `unknown` if git cannot say.
fn git_commit(root: &Path) -> Result<NonEmptyString> {
    let described = Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    NonEmptyString::try_from(described).map_err(|e| anyhow::anyhow!("a commit name: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args> {
        parse_args(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn defaults_are_the_planned_baseline() {
        let args = parse(&[]).expect("no options");
        assert_eq!(args.problems, Vec::<String>::new());
        assert_eq!(args.repeat.get(), 5);
        assert_eq!(args.max_cost_usd, 5.0);
        assert_eq!(args.agent, "main_orchestrator");
        assert_eq!((args.out, args.deadline), (None, None));
    }

    #[test]
    fn options_are_read() {
        let args = parse(&[
            "--problem",
            "part_design_001",
            "--problem",
            "part_design_002",
            "--repeat",
            "2",
            "--max-cost-usd",
            "0.5",
            "--out",
            "x",
            "--deadline-seconds",
            "60",
        ])
        .expect("options");
        assert_eq!(args.problems, ["part_design_001", "part_design_002"]);
        assert_eq!(args.repeat.get(), 2);
        assert_eq!(args.max_cost_usd, 0.5);
        assert_eq!(args.out, Some(PathBuf::from("x")));
        assert_eq!(args.deadline, Some(Duration::from_secs(60)));
    }

    #[test]
    fn a_run_costs_its_models_price_and_the_cap_counts_the_dearest() {
        let root = crate::workspace_root().expect("the workspace");
        let loader = infra::config::ConfigLoader::new(contracts::Environment::Production)
            .expect("the config schemas");
        let routing = Routing::new(
            loader
                .load(&root.join("config/models/routing.yaml"))
                .expect("the routing"),
            Arc::new(model_gateway::providers::default_registry().expect("providers")),
            contracts::Environment::Production,
        )
        .expect("checked");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            tool_calls: 3,
            steps: 4,
            wall_time_seconds: 20.0,
        };
        let id = |m: &str| NonEmptyString::try_from(m.to_owned()).expect("an id");
        // Haiku 5.5: 0.10 per input Mtok, 0.50 per output Mtok.
        let haiku = cost_of(&routing, &[id("claude-haiku-5-5")], &usage);
        assert!(
            (haiku.exact.expect("priced") - 0.15).abs() < 1e-12,
            "{haiku:?}"
        );
        assert_eq!(haiku.exact, Some(haiku.bound));
        let both = cost_of(
            &routing,
            &[id("claude-haiku-5-5"), id("claude-opus-5-5")],
            &usage,
        );
        assert_eq!(both.exact, None, "two models: no exact cost");
        assert!((both.bound - 6.0).abs() < 1e-12, "{both:?}");
        let unpriced = cost_of(&routing, &[id("no-such-model")], &usage);
        assert_eq!(
            unpriced,
            Cost {
                exact: None,
                bound: 0.0
            }
        );
    }

    #[test]
    fn bad_options_are_refused() {
        for bad in [
            &["--repeat", "0"][..],
            &["--repeat"],
            &["--max-cost-usd", "0"],
            &["--max-cost-usd", "NaN"],
            &["--deadline-seconds", "0"],
            &["--fast"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
