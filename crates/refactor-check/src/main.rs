use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use refactor_check::agent::{run, run_with_llm, DEFAULT_SOLVER_TIMEOUT_SECS};
use refactor_check::config_update::{AppConfig, ServiceTierArg};
use refactor_check::live_config::LiveConfig;
use refactor_check::llm::LlmConfig;
use refactor_check::smt::SolverConfig;
use refactor_check_core::codex::{CodexConfig, CodexLlmProvider};

#[derive(Parser)]
#[command(
    name = "refactor-check",
    about = "Check refactoring equivalence via SMT solving"
)]
struct Cli {
    /// Input file containing before/after code
    #[arg(long)]
    input: String,

    /// API endpoint base URL
    #[arg(long, default_value = "https://openrouter.ai/api/v1")]
    api_base: String,

    /// API key (or path to file containing key)
    #[arg(long, env = "OPENROUTER_API_KEY")]
    api_key: Option<String>,

    /// Inference backend: OpenAI-compatible API or authenticated Codex CLI
    #[arg(long, value_enum, default_value = "api")]
    llm_provider: LlmProviderArg,

    /// Primary model (required for API; defaults to local Codex config for Codex)
    #[arg(long, required_if_eq("llm_provider", "api"))]
    primary_model: Option<String>,

    /// Judge model (defaults to primary)
    #[arg(long)]
    judge_model: Option<String>,

    /// Codex executable (Codex provider only)
    #[arg(long, default_value = "codex")]
    codex_path: PathBuf,

    /// Override Codex reasoning effort; otherwise preserve local configuration
    #[arg(long, value_parser = ["minimal", "low", "medium", "high", "xhigh"])]
    codex_reasoning_effort: Option<String>,

    /// Wall-clock timeout per Codex invocation, including prompt delivery
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..))]
    codex_timeout_secs: u64,

    /// Solver binary path
    #[arg(long, default_value = "z3")]
    solver_path: String,

    /// Solver arguments
    #[arg(long, default_value = "-in", num_args = 0.., value_delimiter = ' ')]
    solver_args: Vec<String>,

    /// Solver timeout in seconds
    #[arg(long, default_value_t = DEFAULT_SOLVER_TIMEOUT_SECS)]
    solver_timeout_secs: u64,

    /// Stream timeout in milliseconds
    #[arg(long, default_value_t = 3000)]
    stream_timeout_ms: u64,

    /// Max stream retries
    #[arg(long, default_value_t = 5)]
    max_stream_retries: u32,

    /// Service tier
    #[arg(long, default_value = "auto")]
    service_tier: ServiceTierArg,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LlmProviderArg {
    Api,
    Codex,
}

impl Cli {
    fn validate(self) -> std::result::Result<Self, clap::Error> {
        // Clap's conditional requirements do not apply to defaulted values.
        // Preserve the API model requirement even when --llm-provider is omitted.
        if matches!(self.llm_provider, LlmProviderArg::Api) && self.primary_model.is_none() {
            return Err(Self::command().error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "--primary-model is required for the API backend",
            ));
        }
        Ok(self)
    }
}

impl From<&Cli> for LlmConfig {
    fn from(cli: &Cli) -> Self {
        let primary = cli.primary_model.clone().unwrap_or_default();
        LlmConfig {
            api_key: cli.api_key.clone().unwrap_or_default(),
            judge_api_key: None,
            formalizer_api_key: None,
            fixer_api_key: None,
            splitter_api_key: None,
            splitting_judge_api_key: None,
            analyzer_api_key: None,
            api_base: cli.api_base.clone(),
            formalizer_model: primary.clone(),
            fixer_model: primary.clone(),
            judge_model: cli.judge_model.clone().unwrap_or_else(|| primary.clone()),
            splitting_judge_model: cli.judge_model.clone().unwrap_or_else(|| primary.clone()),
            splitter_model: primary.clone(),
            analyzer_model: primary,
            stream_timeout_ms: cli.stream_timeout_ms,
            max_stream_retries: cli.max_stream_retries,
            service_tier: cli.service_tier.clone().into(),
        }
    }
}

impl From<&Cli> for SolverConfig {
    fn from(cli: &Cli) -> Self {
        SolverConfig {
            solver_path: cli.solver_path.clone(),
            solver_args: cli.solver_args.clone(),
            timeout_secs: cli.solver_timeout_secs,
        }
    }
}

impl From<&Cli> for AppConfig {
    fn from(cli: &Cli) -> Self {
        AppConfig {
            llm: LlmConfig::from(cli),
            solver: SolverConfig::from(cli),
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse().validate().unwrap_or_else(|error| error.exit());
    let config_live = Arc::new(LiveConfig::new(AppConfig::from(&cli)));
    let input = cli.input.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.llm_provider {
        LlmProviderArg::Api => rt.block_on(run(&input, config_live, None))?,
        LlmProviderArg::Codex => {
            let provider = CodexLlmProvider::new(CodexConfig {
                executable: cli.codex_path,
                model: cli.primary_model,
                judge_model: cli.judge_model,
                reasoning_effort: cli.codex_reasoning_effort,
                timeout: Duration::from_secs(cli.codex_timeout_secs),
            })?;
            rt.block_on(run_with_llm(&input, config_live, &provider, None))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_still_requires_a_primary_model() {
        assert!(
            Cli::try_parse_from(["refactor-check", "--input", "input.txt"])
                .and_then(Cli::validate)
                .is_err()
        );
        assert!(Cli::try_parse_from([
            "refactor-check",
            "--input",
            "input.txt",
            "--llm-provider",
            "api"
        ])
        .and_then(Cli::validate)
        .is_err());
        assert!(Cli::try_parse_from([
            "refactor-check",
            "--input",
            "input.txt",
            "--primary-model",
            "api-model"
        ])
        .and_then(Cli::validate)
        .is_ok());
    }

    #[test]
    fn codex_accepts_local_defaults_and_rejects_invalid_options() {
        assert!(Cli::try_parse_from([
            "refactor-check",
            "--input",
            "input.txt",
            "--llm-provider",
            "codex"
        ])
        .and_then(Cli::validate)
        .is_ok());
        for (flag, value) in [
            ("--codex-timeout-secs", "0"),
            ("--codex-reasoning-effort", "unknown"),
        ] {
            assert!(Cli::try_parse_from([
                "refactor-check",
                "--input",
                "input.txt",
                "--llm-provider",
                "codex",
                flag,
                value
            ])
            .is_err());
        }
    }
}
