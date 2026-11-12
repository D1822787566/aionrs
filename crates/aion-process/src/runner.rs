use std::io::{Error, Result};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::containment::ChildContainment;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_POST_PROCESS_DRAIN: Duration = Duration::from_millis(250);

/// Runs one process and buffers its stdout/stderr while it is running.
pub struct CommandRunner {
    command: Command,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    post_process_drain: Duration,
    started: Option<Arc<AtomicBool>>,
}

impl CommandRunner {
    pub fn new(command: Command) -> Self {
        Self {
            command,
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
            post_process_drain: DEFAULT_POST_PROCESS_DRAIN,
            started: None,
        }
    }

    /// Provide bytes that should be written to the child process stdin.
    pub fn stdin_bytes(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Let callers distinguish a spawn failure from an error after the child started.
    pub fn started_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.started = Some(flag);
        self
    }

    pub fn post_process_drain(mut self, drain: Duration) -> Self {
        self.post_process_drain = drain;
        self
    }

    pub async fn run(mut self) -> Result<CommandResult> {
        if self.stdin.is_some() {
            self.command.stdin(Stdio::piped());
        }
        self.command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        ChildContainment::configure(&mut self.command);

        if let Some(flag) = &self.started {
            flag.store(false, Ordering::Release);
        }
        let mut child = self.command.spawn()?;
        if let Some(flag) = &self.started {
            flag.store(true, Ordering::Release);
        }
        let child_id = child.id();
        let containment = ChildContainment::attach(&mut child)?;
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stdout_reader = child
            .stdout
            .take()
            .map(|reader| read_stream(reader, Arc::clone(&stdout)));
        let stderr_reader = child
            .stderr
            .take()
            .map(|reader| read_stream(reader, Arc::clone(&stderr)));
        if let Some(input) = self.stdin.take()
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(&input).await?;
            stdin.shutdown().await?;
        }

        match tokio::time::timeout(self.timeout, child.wait()).await {
            Ok(status) => {
                let status = status?;
                let (stdout_result, stderr_result) = tokio::join!(
                    drain_reader_with_result(stdout_reader, self.post_process_drain),
                    drain_reader_with_result(stderr_reader, self.post_process_drain)
                );
                stdout_result?;
                stderr_result?;

                Ok(CommandResult {
                    exit_code: status.code(),
                    timed_out: false,
                    stdout: take_output(stdout),
                    stderr: take_output(stderr),
                })
            }
            Err(_) => {
                containment.terminate(&mut child, child_id)?;
                if let Ok(status) = tokio::time::timeout(self.post_process_drain, child.wait()).await {
                    status?;
                }
                tokio::join!(
                    drain_reader(stdout_reader, self.post_process_drain),
                    drain_reader(stderr_reader, self.post_process_drain)
                );

                Ok(CommandResult {
                    exit_code: None,
                    timed_out: true,
                    stdout: take_output(stdout),
                    stderr: take_output(stderr),
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn read_stream<R>(mut reader: R, output: Arc<Mutex<Vec<u8>>>) -> JoinHandle<Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];

        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }

            output
                .lock()
                .expect("process output mutex should not be poisoned")
                .extend_from_slice(&buffer[..read]);
        }
    })
}

async fn drain_reader(reader: Option<JoinHandle<Result<()>>>, drain: Duration) {
    let _reader_result = drain_reader_with_result(reader, drain).await;
}

async fn drain_reader_with_result(reader: Option<JoinHandle<Result<()>>>, drain: Duration) -> Result<()> {
    if let Some(mut reader) = reader {
        tokio::select! {
            _ = tokio::time::sleep(drain) => {
                reader.abort();
                let _abort_join_result = reader.await;
                Ok(())
            }
            result = &mut reader => {
                result
                    .map_err(|error| Error::other(format!("process output reader failed: {error}")))?
            }
        }
    } else {
        Ok(())
    }
}

fn take_output(output: Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    output
        .lock()
        .expect("process output mutex should not be poisoned")
        .clone()
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod runner_test;
