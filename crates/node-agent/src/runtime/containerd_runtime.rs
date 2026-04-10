use std::process::Stdio;

use tonic::Status;

#[derive(Clone, Copy)]
pub enum RuntimeCli {
    Nerdctl,
    Docker,
}

impl RuntimeCli {
    pub fn bin(self) -> &'static str {
        match self {
            RuntimeCli::Nerdctl => "nerdctl",
            RuntimeCli::Docker => "docker",
        }
    }
}

pub struct ContainerdRuntime {
    cli: RuntimeCli,
}

impl ContainerdRuntime {
    pub async fn detect() -> Result<Self, Status> {
        for cli in [RuntimeCli::Nerdctl, RuntimeCli::Docker] {
            let out = tokio::process::Command::new(cli.bin())
                .arg("version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .await;
            if let Ok(output) = out {
                if output.status.success() {
                    return Ok(Self { cli });
                }
            }
        }
        Err(Status::failed_precondition(
            "no supported container runtime CLI found; install nerdctl (preferred for containerd) or docker",
        ))
    }

    pub fn cli(&self) -> RuntimeCli {
        self.cli
    }

    pub async fn run(&self, args: &[String]) -> Result<String, Status> {
        let out = tokio::process::Command::new(self.cli.bin())
            .args(args)
            .output()
            .await
            .map_err(|e| Status::internal(format!("running {}: {e}", self.cli.bin())))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Status::failed_precondition(format!(
                "{} {} failed: {}",
                self.cli.bin(),
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}
