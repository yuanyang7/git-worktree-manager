use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub command: String,
}

#[derive(Debug)]
pub enum ProcessError {
    Io(io::Error),
    Command(String),
    InvalidOutput(String),
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "run lsof: {error}"),
            Self::Command(message) => write!(formatter, "lsof failed: {message}"),
            Self::InvalidOutput(message) => write!(formatter, "invalid lsof output: {message}"),
        }
    }
}

impl std::error::Error for ProcessError {}

pub fn active_processes(path: &Path) -> Result<Vec<ProcessInfo>, ProcessError> {
    let output = Command::new("lsof")
        .args(["-Fpc", "-a", "-d", "cwd", "--"])
        .arg(path)
        .output()
        .map_err(ProcessError::Io)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if output.status.code() == Some(1) && message.is_empty() {
            return Ok(Vec::new());
        }
        return Err(ProcessError::Command(if message.is_empty() {
            format!("exit code {:?}", output.status.code())
        } else {
            message
        }));
    }

    parse_lsof_output(&output.stdout)
}

fn parse_lsof_output(output: &[u8]) -> Result<Vec<ProcessInfo>, ProcessError> {
    let mut processes = Vec::new();
    let mut pid = None;
    let mut command = None;
    for field in String::from_utf8_lossy(output).lines() {
        let Some((kind, value)) = field.split_at_checked(1) else {
            continue;
        };
        match kind {
            "p" => {
                if let Some(process) = process_info(pid.take(), command.take())? {
                    processes.push(process);
                }
                pid = Some(value.parse::<u32>().map_err(|error| {
                    ProcessError::InvalidOutput(format!("invalid process id {value:?}: {error}"))
                })?);
            }
            "c" => command = Some(value.to_owned()),
            _ => {}
        }
    }
    if let Some(process) = process_info(pid, command)? {
        processes.push(process);
    }

    let current_pid = std::process::id();
    processes.retain(|process| process.pid != current_pid);
    Ok(processes)
}

fn process_info(
    pid: Option<u32>,
    command: Option<String>,
) -> Result<Option<ProcessInfo>, ProcessError> {
    match (pid, command) {
        (None, None) => Ok(None),
        (Some(pid), Some(command)) => Ok(Some(ProcessInfo { pid, command })),
        _ => Err(ProcessError::InvalidOutput(
            "process record has an incomplete pid/command pair".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_lsof_output;

    #[test]
    fn parses_process_records_and_ignores_unrelated_fields() {
        let output = b"p12\ncworker\nf cwd\np34\ncshell\n";
        assert_eq!(
            parse_lsof_output(output).unwrap(),
            vec![
                super::ProcessInfo {
                    pid: 12,
                    command: "worker".to_owned(),
                },
                super::ProcessInfo {
                    pid: 34,
                    command: "shell".to_owned(),
                },
            ]
        );
    }
}
