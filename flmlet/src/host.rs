//! Linux host effects. Unit tests use a fake Host and never contact systemd.
use crate::{CommandOutput, Error, Host, Result};
use std::{
    io::{Read, Write},
    net::{SocketAddr, SocketAddrV4, TcpStream},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct LocalHost;
impl Host for LocalHost {
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn write_atomic(&mut self, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::Invalid("file needs a parent directory".into()))?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
    fn executable(&mut self, path: &Path) -> bool {
        if path.components().count() == 1 {
            return std::env::var_os("PATH").is_some_and(|paths| {
                std::env::split_paths(&paths)
                    .filter(|entry| entry.is_absolute())
                    .any(|entry| self.executable(&entry.join(path)))
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        }
        #[cfg(not(unix))]
        path.is_file()
    }
    fn command(&mut self, program: &Path, args: &[String]) -> Result<CommandOutput> {
        let timeout = if program == Path::new("systemctl") {
            Duration::from_secs(75)
        } else {
            Duration::from_secs(15)
        };
        self.run(program, args, timeout)
    }
    fn bootstrap(
        &mut self,
        settings: &crate::Settings,
        version: &str,
        python: &str,
        force: bool,
    ) -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let installer = temporary.path().join("setup.sh");
        let fetch = |host: &mut Self, url: &str, path: &Path| -> Result<bool> {
            let args = [
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--retry",
                "2",
                "--connect-timeout",
                "15",
                "--max-time",
                "180",
                "--output",
                path.to_str()
                    .ok_or_else(|| Error::Invalid("invalid temporary path".into()))?,
                url,
            ]
            .map(String::from);
            Ok(host
                .run(Path::new("curl"), &args, Duration::from_secs(195))?
                .success)
        };
        let mut args = vec![
            installer.to_string_lossy().into_owned(),
            "--join".into(),
            "--no-systemd".into(),
            "--prefix".into(),
            settings.prefix.to_string_lossy().into_owned(),
            "--python-version".into(),
            python.into(),
            "--version".into(),
            version.into(),
        ];
        if force {
            args.push("--force".into());
        }
        {
            // Use the installer bundled in the checksum-verified official release.
            // This also works when the hosted setup.sh endpoint is unavailable.
            use sha2::{Digest, Sha256};
            let arch = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                _ => {
                    return Err(Error::Invalid(
                        "unsupported Flame runtime architecture".into(),
                    ));
                }
            };
            let name = format!("flame-{version}-linux-{arch}");
            let url = format!(
                "https://github.com/xflops/flame/releases/download/{version}/{name}.tar.gz"
            );
            let archive = temporary.path().join(format!("{name}.tar.gz"));
            let checksum = temporary.path().join(format!("{name}.tar.gz.sha256"));
            if !fetch(self, &url, &archive)? || !fetch(self, &format!("{url}.sha256"), &checksum)? {
                return Err(Error::Command(
                    "could not download the official Flame installer/runtime".into(),
                ));
            }
            let expected = std::fs::read_to_string(&checksum)?;
            let expected = expected.split_whitespace().next().unwrap_or_default();
            let actual = format!("{:x}", Sha256::digest(std::fs::read(&archive)?));
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(Error::Command("Flame runtime checksum mismatch".into()));
            }
            let extracted = self.run(
                Path::new("tar"),
                &[
                    "-xOf".into(),
                    archive.to_string_lossy().into_owned(),
                    format!("{name}/sbin/setup.sh"),
                ],
                Duration::from_secs(30),
            )?;
            if !extracted.success {
                return Err(Error::Command(
                    "official Flame archive has no installer".into(),
                ));
            }
            std::fs::write(&installer, extracted.stdout)?;
            args.extend(["--package".into(), archive.to_string_lossy().into_owned()]);
        }
        if !self
            .run(Path::new("sh"), &args, Duration::from_secs(1800))?
            .success
        {
            return Err(Error::Command("Flame runtime installation failed".into()));
        }
        Ok(())
    }
    fn hostname(&mut self) -> Result<String> {
        if let Ok(value) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.into());
            }
        }
        let result = self.command(Path::new("hostname"), &[])?;
        if !result.success || result.stdout.trim().is_empty() {
            return Err(Error::Command(
                "cannot determine the actual host name".into(),
            ));
        }
        Ok(result.stdout.trim().into())
    }
    fn tcp_ready(&mut self, address: SocketAddrV4) -> bool {
        TcpStream::connect_timeout(&SocketAddr::V4(address), Duration::from_secs(2)).is_ok()
    }
}

impl LocalHost {
    fn run(&mut self, program: &Path, args: &[String], timeout: Duration) -> Result<CommandOutput> {
        let mut process = Command::new(program);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        let mut child = process
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        // Drain both pipes concurrently so a large CLI inventory cannot deadlock wait().
        let out = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(4 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let err = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(1024 * 1024)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break Err(Error::Command(format!("{} timed out", program.display())));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Close any descendants' inherited pipes as well, even if their parent exited.
        #[cfg(unix)]
        {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .stderr(Stdio::null())
                .status();
        }
        let stdout = out
            .join()
            .map_err(|_| Error::Command("stdout reader failed".into()))??;
        let stderr = err
            .join()
            .map_err(|_| Error::Command("stderr reader failed".into()))??;
        Ok(CommandOutput {
            success: status?.success(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_configuration_replaces_contents_with_private_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conf/generated.yaml");
        let mut host = LocalHost;
        host.write_atomic(&path, b"first", 0o600).unwrap();
        host.write_atomic(&path, b"second", 0o600).unwrap();
        assert_eq!(host.read(&path).unwrap().unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}
