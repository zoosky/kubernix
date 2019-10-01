use crate::{
    process::{Process, Startable, Stoppable},
    Config,
};
use failure::{bail, format_err, Fallible};
use log::{debug, info};
use serde_json::{json, to_string_pretty};
use std::{
    env,
    fs::{self, create_dir_all},
    path::{Path, PathBuf},
    process::Command,
};

pub struct Crio {
    process: Process,
    socket: PathBuf,
}

impl Crio {
    pub fn start(config: &Config, socket: &Path) -> Fallible<Startable> {
        info!("Starting CRI-O");
        let conmon = Self::find_executable("conmon")
            .ok_or_else(|| format_err!("Unable to find conmon in $PATH"))?;

        let bridge = Self::find_executable("bridge")
            .ok_or_else(|| format_err!("Unable to find CNI bridge in $PATH"))?;
        let cni = bridge
            .parent()
            .ok_or_else(|| format_err!("Unable to find CNI plugin dir"))?;

        let dir = config.root.join(&config.crio.dir);
        create_dir_all(&dir)?;

        let cni_config = dir.join("cni");
        create_dir_all(&cni_config)?;
        let bridge_json = cni_config.join("bridge.json");
        fs::write(
            bridge_json,
            to_string_pretty(&json!({
              "cniVersion": "0.3.1",
              "name": "crio-bridge",
              "type": "bridge",
              "bridge": "cni0",
              "isGateway": true,
              "ipMasq": true,
              "hairpinMode": true,
              "ipam": {
                "type": "host-local",
                "routes": [{ "dst": "0.0.0.0/0" }],
                "ranges": [[{ "subnet": config.crio.cidr }]]
              }
            }))?,
        )?;

        let policy_json = dir.join("policy.json");
        fs::write(
            &policy_json,
            to_string_pretty(&json!({
              "default": [{
                  "type": "insecureAcceptAnything"
              }]
            }))?,
        )?;

        let runc = Self::find_executable("runc")
            .ok_or_else(|| format_err!("Unable to find runc in $PATH"))?;
        let storage_driver = "overlay".to_owned();
        let storage_root = dir.join("storage");
        let run_root = dir.join("run");
        let mut process = Process::start(
            config,
            &[
                "crio".to_owned(),
                "--log-level=debug".to_owned(),
                format!("--storage-driver={}", &storage_driver),
                format!("--conmon={}", conmon.display()),
                format!("--listen={}", &socket.display()),
                format!("--root={}", &storage_root.display()),
                format!("--runroot={}", &run_root.display()),
                format!("--cni-config-dir={}", cni_config.display()),
                format!("--cni-plugin-dir={}", cni.display()),
                "--registry=docker.io".to_owned(),
                format!("--signature-policy={}", policy_json.display()),
                format!(
                    "--runtimes=local-runc:{}:{}",
                    runc.display(),
                    dir.join("runc").display()
                ),
                "--default-runtime=local-runc".to_owned(),
            ],
        )?;

        process.wait_ready("sandboxes:")?;
        info!("CRI-O is ready");
        Ok(Box::new(Crio {
            process,
            socket: socket.to_path_buf(),
        }))
    }

    fn find_executable<P>(name: P) -> Option<PathBuf>
    where
        P: AsRef<Path>,
    {
        env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .filter_map(|dir| {
                    let full_path = dir.join(&name);
                    if full_path.is_file() {
                        Some(full_path)
                    } else {
                        None
                    }
                })
                .next()
        })
    }

    fn remove_all_containers(&self) -> Fallible<()> {
        debug!("Removing all CRI-O workloads");
        let env_key = "CONTAINER_RUNTIME_ENDPOINT";
        let env_value = format!("unix://{}", self.socket.display());

        let output = Command::new("crictl")
            .env(env_key, &env_value)
            .arg("pods")
            .arg("-q")
            .output()?;
        let stdout = String::from_utf8(output.stdout)?;
        if !output.status.success() {
            debug!("critcl stdout: {}", stdout);
            debug!("critcl stderr: {}", String::from_utf8(output.stderr)?);
            bail!("crictl pods command failed");
        }

        for x in stdout.lines() {
            debug!("Removing pod {}", x);
            let output = Command::new("crictl")
                .env(env_key, &env_value)
                .arg("rmp")
                .arg("-f")
                .arg(x)
                .output()?;
            if !output.status.success() {
                debug!("critcl stdout: {}", String::from_utf8(output.stdout)?);
                debug!("critcl stderr: {}", String::from_utf8(output.stderr)?);
                bail!("crictl rmp command failed");
            }
        }

        debug!("All workloads removed");
        Ok(())
    }
}

impl Stoppable for Crio {
    fn stop(&mut self) -> Fallible<()> {
        // Remove all running containers
        self.remove_all_containers()?;

        // Stop the process
        self.process.stop()
    }
}
