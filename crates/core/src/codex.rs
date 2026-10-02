//! Text inference through an authenticated local `codex exec` installation.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::error_gate::ErrorGate;
use crate::llm::{Message, Role};
use crate::provider::{IOProvider, LlmRequest, LlmRole, WithContext};

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub executable: PathBuf,
    /// None preserves the model configured in Codex itself.
    pub model: Option<String>,
    pub judge_model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("codex"),
            model: None,
            judge_model: None,
            reasoning_effort: None,
            timeout: Duration::from_secs(300),
        }
    }
}

pub struct CodexLlmProvider {
    config: CodexConfig,
    error_gate: Option<Arc<ErrorGate>>,
}

impl CodexLlmProvider {
    pub fn new(config: CodexConfig) -> Result<Self> {
        if config.timeout.is_zero() {
            bail!("Codex timeout must be greater than zero");
        }
        Ok(Self {
            config,
            error_gate: None,
        })
    }

    pub fn with_error_gate(mut self, gate: Arc<ErrorGate>) -> Self {
        self.error_gate = Some(gate);
        self
    }

    fn model_for(&self, role: LlmRole) -> Option<&str> {
        match role {
            LlmRole::Judge | LlmRole::SplittingJudge => self
                .config
                .judge_model
                .as_deref()
                .or(self.config.model.as_deref()),
            _ => self.config.model.as_deref(),
        }
    }

    async fn infer(&self, input: &LlmRequest) -> Result<String> {
        let context_id = &input.context_id;
        let workspace = tempfile::Builder::new()
            .prefix("refactor-check-codex-")
            .tempdir()
            .context("Failed to create Codex inference directory")?;
        let response_path = workspace.path().join("response.txt");
        let mut command = tokio::process::Command::new(&self.config.executable);
        command.args([
            "exec",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--color",
            "never",
            "--config",
            "project_doc_max_bytes=0",
            "--config",
            "features.shell_tool=false",
            "--config",
            "web_search=\"disabled\"",
        ]);
        command.arg("--output-last-message").arg(&response_path);
        if let Some(model) = self.model_for(input.role) {
            command.arg("--model").arg(model);
        }
        if let Some(effort) = &self.config.reasoning_effort {
            command.arg("--config").arg(format!(
                "model_reasoning_effort={}",
                serde_json::to_string(effort)?
            ));
        }
        command
            .arg("-")
            .current_dir(workspace.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        info!(%context_id, role = ?input.role, model = ?self.model_for(input.role), "starting Codex inference");
        let prompt = inference_prompt(&input.messages)?;
        let started = std::time::Instant::now();
        // The timeout includes stdin delivery, process exit, and response validation.
        // Dropping wait_with_output on timeout/cancellation kills the direct child.
        let operation = async {
            let mut child = command
                .spawn()
                .context("Failed to start Codex; install the CLI and run codex login")?;
            let mut stdin = child.stdin.take().context("Failed to open Codex stdin")?;
            // Drain stderr concurrently with writing large prompts to avoid pipe deadlock.
            let write_prompt = async move {
                stdin
                    .write_all(prompt.as_bytes())
                    .await
                    .context("Failed to send Codex prompt")?;
                stdin
                    .shutdown()
                    .await
                    .context("Failed to close Codex stdin")
            };
            let ((), output) = tokio::try_join!(write_prompt, async {
                child
                    .wait_with_output()
                    .await
                    .context("Failed to wait for Codex")
            })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let diagnostic: String = stderr
                    .chars()
                    .rev()
                    .take(2000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                bail!("Codex exited with {}: {}", output.status, diagnostic.trim());
            }
            let response = std::fs::read_to_string(&response_path)
                .context("Codex succeeded but did not write a UTF-8 final response")?;
            if response.trim().is_empty() {
                bail!("Codex returned an empty final response");
            }
            Ok(response)
        };
        let response = tokio::time::timeout(self.config.timeout, operation)
            .await
            .with_context(|| {
                format!(
                    "Codex inference timed out after {} seconds",
                    self.config.timeout.as_secs()
                )
            })??;
        debug!(%context_id, elapsed_ms = started.elapsed().as_millis(), response_bytes = response.len(), "Codex inference complete");
        Ok(response)
    }
}

fn inference_prompt(messages: &[Message]) -> Result<String> {
    let conversation: Vec<_> = messages
        .iter()
        .map(|message| {
            let role = match message.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            serde_json::json!({"role": role, "content": message.content})
        })
        .collect();
    Ok(format!(
        "Act as a text-only inference provider for a formal-verification pipeline. \
         Follow the system instructions in the JSON conversation below and answer its final user request. \
         Do not use tools, inspect files, execute commands, or alter the workspace. \
         Return only the requested response, preserving code fences and exact verdict words.\n\n{}",
        serde_json::to_string(&conversation)?,
    ))
}

#[async_trait]
impl IOProvider<LlmRequest, WithContext<String>> for CodexLlmProvider {
    async fn invoke(&self, input: LlmRequest) -> Result<WithContext<String>> {
        loop {
            match self.infer(&input).await {
                Ok(value) => {
                    return Ok(WithContext {
                        value,
                        context_id: input.context_id,
                    })
                }
                Err(error) => {
                    // Missing executables or required response files are setup/protocol
                    // failures, not requests to park indefinitely at the error gate.
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|cause| cause.kind() == std::io::ErrorKind::NotFound)
                    {
                        return Err(error);
                    }
                    if let Some(gate) = &self.error_gate {
                        gate.report_and_wait(&format!("{error:#}")).await?;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::context_id::ContextId;
    use std::os::unix::fs::PermissionsExt;

    fn fake_codex(body: &str) -> (tempfile::TempDir, CodexConfig) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake codex");
        std::fs::write(&executable, format!("#!/usr/bin/env python3\nimport json,os,sys,time\nfrom pathlib import Path\n{body}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = CodexConfig {
            executable,
            timeout: Duration::from_secs(10),
            ..CodexConfig::default()
        };
        (directory, config)
    }

    fn request(role: LlmRole) -> LlmRequest {
        LlmRequest {
            role,
            messages: vec![
                Message {
                    role: Role::System,
                    content: "Only output SMT.\nKeep ``` fences.".into(),
                },
                Message {
                    role: Role::User,
                    content: "x = \"$(touch forbidden)\";\nUnicode: 中文".into(),
                },
            ],
            context_id: Box::new(ContextId::root().new_child()),
        }
    }

    #[tokio::test]
    async fn transports_prompt_and_model_as_data_and_preserves_context() {
        let (_directory, mut config) = fake_codex(
            "data=sys.stdin.read()\nargs=sys.argv[1:]\np=Path(args[args.index('--output-last-message')+1])\np.write_text(json.dumps({'args':args,'prompt':data,'cwd':os.getcwd()}))\nprint('NOT THE RESPONSE',file=sys.stderr)\nprint('ALSO NOT THE RESPONSE')",
        );
        config.model = Some("model with spaces; $(touch forbidden)".into());
        config.reasoning_effort = Some("high".into());
        let provider = CodexLlmProvider::new(config).unwrap();
        let input = request(LlmRole::Formalizer);
        let context_id = input.context_id.hash();
        let original_messages = input.messages.clone();
        let output = provider.invoke(input).await.unwrap();
        assert_eq!(output.context_id.hash(), context_id);
        let data: serde_json::Value = serde_json::from_str(&output.value).unwrap();
        let args = data["args"].as_array().unwrap();
        let model_index = args.iter().position(|arg| arg == "--model").unwrap();
        assert_eq!(
            args[model_index + 1],
            "model with spaces; $(touch forbidden)"
        );
        let prompt = data["prompt"].as_str().unwrap();
        let (_, encoded) = prompt.split_once("\n\n").unwrap();
        let conversation: serde_json::Value = serde_json::from_str(encoded).unwrap();
        assert_eq!(conversation[0]["role"], "system");
        assert_eq!(conversation[0]["content"], original_messages[0].content);
        assert_eq!(conversation[1]["content"], original_messages[1].content);
        assert!(
            !PathBuf::from(data["cwd"].as_str().unwrap()).exists(),
            "inference directory should be cleaned"
        );
    }

    #[test]
    fn model_routing_preserves_codex_defaults_and_separates_judges() {
        let provider = CodexLlmProvider::new(CodexConfig::default()).unwrap();
        assert_eq!(provider.model_for(LlmRole::Judge), None);
        let provider = CodexLlmProvider::new(CodexConfig {
            model: Some("primary".into()),
            judge_model: Some("judge".into()),
            ..CodexConfig::default()
        })
        .unwrap();
        for role in [LlmRole::Judge, LlmRole::SplittingJudge] {
            assert_eq!(provider.model_for(role), Some("judge"));
        }
        for role in [
            LlmRole::Splitter,
            LlmRole::Formalizer,
            LlmRole::Fixer,
            LlmRole::Analyzer,
        ] {
            assert_eq!(provider.model_for(role), Some("primary"));
        }
    }

    #[tokio::test]
    async fn preserves_fences_and_never_returns_cli_progress() {
        let (_directory, config) = fake_codex(
            "sys.stdin.read()\np=Path(sys.argv[sys.argv.index('--output-last-message')+1])\np.write_text('```smt2\\n(check-sat)\\n```\\n')\nprint('progress on stdout')\nprint('progress on stderr',file=sys.stderr)",
        );
        let result = CodexLlmProvider::new(config)
            .unwrap()
            .invoke(request(LlmRole::Formalizer))
            .await
            .unwrap();
        assert_eq!(result.value, "```smt2\n(check-sat)\n```\n");
    }

    #[tokio::test]
    async fn rejects_failed_empty_and_missing_final_responses() {
        for body in [
            "sys.stdin.read()\np=Path(sys.argv[sys.argv.index('--output-last-message')+1])\np.write_text('stale-looking success')\nprint('authentication rejected',file=sys.stderr)\nsys.exit(7)",
            "sys.stdin.read()\np=Path(sys.argv[sys.argv.index('--output-last-message')+1])\np.write_text('  \\n')",
            "sys.stdin.read()\nprint('not a final response')",
        ] {
            let (_directory, config) = fake_codex(body);
            assert!(CodexLlmProvider::new(config).unwrap().invoke(request(LlmRole::Judge)).await.is_err());
        }
        let provider = CodexLlmProvider::new(CodexConfig {
            executable: PathBuf::from("/nonexistent/refactor-check-codex"),
            ..CodexConfig::default()
        })
        .unwrap();
        assert!(provider.invoke(request(LlmRole::Judge)).await.is_err());
    }

    #[tokio::test]
    async fn timeout_includes_stdin_delivery_and_reaps_direct_child() {
        let (_directory, mut config) = fake_codex("");
        // Record readiness before doing anything else. Concurrent fresh process
        // startup on macOS can exceed one second, so include a realistic launch
        // allowance while keeping the never-read stdin and child-exit checks.
        std::fs::write(
            &config.executable,
            "#!/bin/sh\nprintf '%s' \"$$\" >\"$0.pid\"\nexec sleep 30\n",
        )
        .unwrap();
        // The fake never reads stdin. A large prompt fills the pipe: timing only
        // wait_with_output would hang indefinitely before reaching the timeout.
        let timeout = Duration::from_secs(5);
        config.timeout = timeout;
        let pid_path = config.executable.with_extension("pid");
        let mut input = request(LlmRole::Formalizer);
        input.messages[1].content = "x".repeat(2_000_000);
        let started = std::time::Instant::now();
        let error = CodexLlmProvider::new(config)
            .unwrap()
            .invoke(input)
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(started.elapsed() >= timeout);
        assert!(started.elapsed() < timeout + Duration::from_secs(3));
        let pid = std::fs::read_to_string(&pid_path).unwrap_or_else(|error| {
            let entries: Vec<_> = std::fs::read_dir(_directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            panic!("missing pid at {pid_path:?}: {error}; files={entries:?}");
        });
        for _ in 0..100 {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out Codex child was not reaped");
    }

    #[tokio::test]
    async fn missing_dependency_fails_without_parking_at_the_error_gate() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        use std::sync::{mpsc, Mutex};
        let parked = Arc::new(Mutex::new(Vec::new()));
        let (sender, _receiver) = mpsc::channel();
        let gate = Arc::new(ErrorGate::new(
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            sender,
            parked.clone(),
        ));
        let provider = CodexLlmProvider::new(CodexConfig {
            executable: PathBuf::from("/nonexistent/refactor-check-codex"),
            ..CodexConfig::default()
        })
        .unwrap()
        .with_error_gate(gate);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            provider.invoke(request(LlmRole::Judge)),
        )
        .await;
        assert!(result.unwrap().is_err());
        assert!(parked.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_zero_timeout() {
        assert!(CodexLlmProvider::new(CodexConfig {
            timeout: Duration::ZERO,
            ..CodexConfig::default()
        })
        .is_err());
    }
}
