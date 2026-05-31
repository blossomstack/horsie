//! Thin unix-socket client for the October daemon. Each call opens a fresh
//! connection, sends one [`DaemonRequest`], and reads the response(s).

use crate::daemon::protocol::{read_frame, write_frame};
use crate::daemon::socket_path;
use crate::error::CliError;
use models::daemon::{
    DaemonRequest, DaemonResponse, JobStatus, JobSummary, ListRequest, LogsRequest, RemoveRequest,
    ResumeRequest, ShutdownRequest, StatusInfo, StatusRequest, StopRequest, SubmitRequest,
};
use std::io::Write;
use std::path::Path;
use tokio::net::UnixStream;

async fn connect(root: &Path) -> Result<UnixStream, CliError> {
    UnixStream::connect(socket_path(root)).await.map_err(|_| {
        CliError::Executor("no daemon running; start it with `october daemon start`".to_string())
    })
}

async fn request(root: &Path, req: &DaemonRequest) -> Result<DaemonResponse, CliError> {
    let mut stream = connect(root).await?;
    write_frame(&mut stream, req)
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?;
    read_frame(&mut stream)
        .await
        .map_err(|e| CliError::Executor(e.to_string()))?
        .ok_or_else(|| CliError::Executor("daemon closed connection".to_string()))
}

/// Turn a non-expected response into an error. Every variant is enumerated (no
/// wildcard arm) so adding a protocol variant forces a compile error here rather
/// than silent mishandling. An `Error` response carries the daemon's own message.
fn unexpected(resp: DaemonResponse) -> CliError {
    let label = match resp {
        DaemonResponse::Error(e) => return CliError::Executor(e.message),
        DaemonResponse::Submitted(_) => "submitted",
        DaemonResponse::JobList(_) => "job-list",
        DaemonResponse::Ack(_) => "ack",
        DaemonResponse::Status(_) => "status",
        DaemonResponse::LogFrame(_) => "log-frame",
        DaemonResponse::End(_) => "end",
    };
    CliError::Executor(format!("unexpected daemon response: {label}"))
}

/// Submit a job; returns its id.
pub async fn submit(root: &Path, req: SubmitRequest) -> Result<String, CliError> {
    let resp = request(root, &DaemonRequest::Submit(req)).await?;
    if let DaemonResponse::Submitted(s) = resp {
        Ok(s.job_id)
    } else {
        Err(unexpected(resp))
    }
}

pub async fn list(root: &Path) -> Result<Vec<JobSummary>, CliError> {
    let resp = request(root, &DaemonRequest::List(ListRequest {})).await?;
    if let DaemonResponse::JobList(l) = resp {
        Ok(l.jobs)
    } else {
        Err(unexpected(resp))
    }
}

pub async fn status(root: &Path) -> Result<StatusInfo, CliError> {
    let resp = request(root, &DaemonRequest::Status(StatusRequest {})).await?;
    if let DaemonResponse::Status(s) = resp {
        Ok(s)
    } else {
        Err(unexpected(resp))
    }
}

pub async fn stop(root: &Path, job_id: String) -> Result<(), CliError> {
    let resp = request(root, &DaemonRequest::Stop(StopRequest { job_id })).await?;
    if let DaemonResponse::Ack(_) = resp {
        Ok(())
    } else {
        Err(unexpected(resp))
    }
}

pub async fn resume(root: &Path, job_id: String, message: String) -> Result<(), CliError> {
    let resp = request(
        root,
        &DaemonRequest::Resume(ResumeRequest { job_id, message }),
    )
    .await?;
    if let DaemonResponse::Ack(_) = resp {
        Ok(())
    } else {
        Err(unexpected(resp))
    }
}

pub async fn remove(root: &Path, job_id: String) -> Result<(), CliError> {
    let resp = request(root, &DaemonRequest::Remove(RemoveRequest { job_id })).await?;
    if let DaemonResponse::Ack(_) = resp {
        Ok(())
    } else {
        Err(unexpected(resp))
    }
}

/// Stop the daemon. With `drain`, the daemon waits for running jobs to finish
/// before exiting.
pub async fn shutdown(root: &Path, drain: bool) -> Result<(), CliError> {
    let resp = request(root, &DaemonRequest::Shutdown(ShutdownRequest { drain })).await?;
    if let DaemonResponse::Ack(_) = resp {
        Ok(())
    } else {
        Err(unexpected(resp))
    }
}

/// Stream a job's logs to stdout until the daemon sends `End`. With `follow`, the
/// daemon keeps the stream open until the job ends.
pub async fn logs(root: &Path, job_id: String, follow: bool) -> Result<(), CliError> {
    let mut stream = connect(root).await?;
    write_frame(
        &mut stream,
        &DaemonRequest::Logs(LogsRequest { job_id, follow }),
    )
    .await
    .map_err(|e| CliError::Executor(e.to_string()))?;
    loop {
        let frame: Option<DaemonResponse> = read_frame(&mut stream)
            .await
            .map_err(|e| CliError::Executor(e.to_string()))?;
        let Some(resp) = frame else { break };
        match resp {
            DaemonResponse::LogFrame(f) => {
                print!("{}", f.text);
                let _ = std::io::stdout().flush();
            }
            DaemonResponse::End(_) => break,
            DaemonResponse::Error(e) => return Err(CliError::Executor(e.message)),
            DaemonResponse::Submitted(_)
            | DaemonResponse::JobList(_)
            | DaemonResponse::Ack(_)
            | DaemonResponse::Status(_) => {
                return Err(CliError::Executor(
                    "unexpected frame in log stream".to_string(),
                ));
            }
        }
    }
    Ok(())
}

/// Submit a job and stream its output until it ends, returning a process exit code
/// (0 = finished/suspended/awaiting, 1 = failed).
pub async fn run_attached(root: &Path, req: SubmitRequest) -> Result<i32, CliError> {
    let job_id = submit(root, req).await?;
    println!("job {job_id}");
    logs(root, job_id.clone(), true).await?;
    // Report the terminal status as an exit code.
    let status = list(root)
        .await?
        .into_iter()
        .find(|j| j.job_id == job_id)
        .map(|j| j.status);
    Ok(match status {
        Some(JobStatus::Failed) => 1,
        Some(JobStatus::Running)
        | Some(JobStatus::Suspended)
        | Some(JobStatus::AwaitingUserInput)
        | Some(JobStatus::Finished)
        | None => 0,
    })
}
