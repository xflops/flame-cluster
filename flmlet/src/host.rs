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
        let mut child = Command::new(program)
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
