use super::env::EnvironmentPolicy;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind { Captured, Logged, Detached }

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub policy: EnvironmentPolicy,
    pub kind: ExecutionKind,
    pub log_path: Option<PathBuf>,
    pub new_process_group: bool,
}

impl ProcessSpec {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            arguments: Vec::new(),
            working_directory: None,
            policy: EnvironmentPolicy::new(),
            kind: ExecutionKind::Captured,
            log_path: None,
            new_process_group: false,
        }
    }

    pub fn arg(mut self, argument: impl AsRef<std::ffi::OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_string_lossy().into_owned());
        self
    }
    pub fn args<I, S>(mut self, arguments: I) -> Self where I: IntoIterator<Item = S>, S: AsRef<std::ffi::OsStr> {
        for argument in arguments { self.arguments.push(argument.as_ref().to_string_lossy().into_owned()); }
        self
    }
    pub fn cwd(mut self, directory: impl Into<PathBuf>) -> Self { self.working_directory = Some(directory.into()); self }
    pub fn policy(mut self, policy: EnvironmentPolicy) -> Self { self.policy = policy; self }
    pub fn kind(mut self, kind: ExecutionKind) -> Self { self.kind = kind; self }
    pub fn logged(mut self, path: impl Into<PathBuf>) -> Self { self.kind = ExecutionKind::Logged; self.log_path = Some(path.into()); self }
    pub fn log_path(mut self, path: impl Into<PathBuf>) -> Self { self.log_path = Some(path.into()); self.kind = ExecutionKind::Logged; self }
    pub fn detached(mut self) -> Self { self.kind = ExecutionKind::Detached; self.new_process_group = true; self }
    pub fn new_process_group(mut self, enabled: bool) -> Self { self.new_process_group = enabled; self }

    pub fn executable_path(&self) -> &Path { &self.executable }
}
