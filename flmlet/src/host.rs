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

fn invalid_github_mirror() -> Error {
    Error::Invalid("FLAME_GITHUB_MIRROR must be an absolute HTTPS URL prefix without credentials, query, or fragment".into())
}

/// Only the archive may use an explicitly selected accelerator.
fn runtime_download_urls(official: &str, mirror: Option<&str>) -> Result<(String, String)> {
    let archive = if let Some(prefix) = mirror {
        let uri: http::Uri = prefix.parse().map_err(|_| invalid_github_mirror())?;
        let authority = uri.authority().ok_or_else(invalid_github_mirror)?;
        if uri.scheme_str() != Some("https")
            || authority.host().is_empty()
            || authority.as_str().contains(['@', '%', '\\'])
            || (authority.as_str() != authority.host() && authority.port_u16().is_none())
            || authority.port_u16() == Some(0)
            || uri.query().is_some()
            || prefix.contains(['#', '\\'])
        {
            return Err(invalid_github_mirror());
        }
        format!("{}/{official}", prefix.trim_end_matches('/'))
    } else {
        official.to_owned()
    };
    Ok((archive, format!("{official}.sha256")))
}

fn runtime_checksum(
    pin: Option<&std::ffi::OsStr>,
    checksum: &Path,
    fetch_official: impl FnOnce(&Path) -> Result<bool>,
) -> Result<String> {
    let validate = |value: &str| {
        if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(value.to_ascii_lowercase())
        } else {
            Err(Error::Invalid(
                "FLAME_RUNTIME_SHA256 must contain exactly 64 hexadecimal characters".into(),
            ))
        }
    };
    if let Some(pin) = pin {
        return validate(pin.to_str().unwrap_or_default());
    }
    if !fetch_official(checksum)? {
        return Err(Error::Command(
            "could not download the official Flame installer/runtime".into(),
        ));
    }
    let expected = std::fs::read_to_string(checksum)?;
    Ok(expected
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase())
}

fn verify_runtime_archive(archive: &Path, expected: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
    let actual = format!("{:x}", Sha256::digest(std::fs::read(archive)?));
    if actual != expected {
        return Err(Error::Command("Flame runtime checksum mismatch".into()));
    }
    Ok(())
}

fn installer_arguments(
    installer: &Path,
    settings: &crate::Settings,
    version: &str,
    python: &str,
    force: bool,
) -> Vec<String> {
    let mut args = vec![
        format!("FLAME_SYSTEMD_DIR={}", settings.systemd_dir.display()),
        "FLAME_SYSTEMCTL=true".into(),
        "sh".into(),
        installer.to_string_lossy().into_owned(),
        "--join".into(),
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
    args
}

impl Host for LocalHost {
    fn read(&mut self, path: &Path) -> Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn read_dir(&mut self, path: &Path) -> Result<Vec<std::path::PathBuf>> {
        let directory = match std::fs::read_dir(path) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = directory
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths.sort();
        Ok(paths)
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
        let mirror = match std::env::var("FLAME_GITHUB_MIRROR") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(std::env::VarError::NotUnicode(_)) => return Err(invalid_github_mirror()),
        };
        let temporary = tempfile::tempdir()?;
        let installer = temporary.path().join("setup.sh");
        let fetch = |host: &mut Self, url: &str, path: &Path| -> Result<bool> {
            // Runtime archives exceed 60 MB; slow links need more than three minutes.
            // The process deadline bounds all attempts, including an in-flight retry.
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
                "900",
                "--retry-max-time",
                "900",
                "--output",
                path.to_str()
                    .ok_or_else(|| Error::Invalid("invalid temporary path".into()))?,
                url,
            ]
            .map(String::from);
            Ok(host
                .run(Path::new("curl"), &args, Duration::from_secs(930))?
                .success)
        };
        let mut args = installer_arguments(&installer, settings, version, python, force);
        {
            // Use the installer bundled in the checksum-verified official release.
            // This also works when the hosted setup.sh endpoint is unavailable.
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
            let (archive_url, checksum_url) = runtime_download_urls(&url, mirror.as_deref())?;
            let archive = temporary.path().join(format!("{name}.tar.gz"));
            let checksum = temporary.path().join(format!("{name}.tar.gz.sha256"));
            let pin = std::env::var_os("FLAME_RUNTIME_SHA256");
            let expected = runtime_checksum(pin.as_deref(), &checksum, |path| {
                fetch(self, &checksum_url, path)
            })?;
            if !fetch(self, &archive_url, &archive)? {
                return Err(Error::Command(
                    "could not download the official Flame installer/runtime".into(),
                ));
            }
            verify_runtime_archive(&archive, &expected)?;
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
            .run(Path::new("env"), &args, Duration::from_secs(1800))?
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
    const OFFICIAL_ARCHIVE: &str =
        "https://github.com/xflops/flame/releases/download/latest/flame-latest-linux-amd64.tar.gz";

    #[test]
    fn official_runtime_downloads_remain_the_default() {
        let (archive, checksum) = runtime_download_urls(OFFICIAL_ARCHIVE, None).unwrap();
        assert_eq!(archive, OFFICIAL_ARCHIVE);
        assert_eq!(checksum, format!("{OFFICIAL_ARCHIVE}.sha256"));
    }

    #[test]
    fn trusted_checksum_pin_skips_fetch_and_needs_no_checksum_file() {
        let temporary = tempfile::tempdir().unwrap();
        let checksum = temporary.path().join("absent.sha256");
        let pin = "AbCd".repeat(16);
        let expected = runtime_checksum(Some(std::ffi::OsStr::new(&pin)), &checksum, |_| {
            panic!("a trusted pin must skip checksum network access")
        })
        .unwrap();
        assert_eq!(expected, pin.to_ascii_lowercase());
        assert!(!checksum.exists());
    }

    #[test]
    fn absent_pin_fetches_official_checksum_and_propagates_download_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let checksum = temporary.path().join("runtime.sha256");
        let digest = "ABCD".repeat(16);
        let expected = runtime_checksum(None, &checksum, |path| {
            assert_eq!(path, checksum);
            assert!(!path.exists());
            std::fs::write(path, format!("{digest}  runtime.tar.gz\n"))?;
            Ok(true)
        })
        .unwrap();
        assert_eq!(expected, digest.to_ascii_lowercase());
        assert!(runtime_checksum(None, &checksum, |_| Ok(false)).is_err());
    }

    #[test]
    fn invalid_checksum_pins_fail_before_fetch_without_echoing_input() {
        use std::ffi::OsString;
        let mut invalid = [
            "",
            "private-value",
            &"a".repeat(63),
            &"a".repeat(65),
            &format!("{}g", "a".repeat(63)),
            &format!("{}\n", "a".repeat(64)),
        ]
        .map(OsString::from)
        .to_vec();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            invalid.push(OsString::from_vec(vec![0xff; 64]));
        }
        for pin in invalid {
            let error = runtime_checksum(Some(&pin), Path::new("unused.sha256"), |_| {
                panic!("an invalid pin must fail before network access")
            })
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                Error::Invalid(
                    "FLAME_RUNTIME_SHA256 must contain exactly 64 hexadecimal characters".into()
                )
                .to_string()
            );
        }
    }

    #[test]
    fn trusted_pin_still_requires_archive_content_to_match() {
        let temporary = tempfile::tempdir().unwrap();
        let archive = temporary.path().join("runtime.tar.gz");
        std::fs::write(&archive, b"abc").unwrap();
        let pin = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let expected = runtime_checksum(
            Some(std::ffi::OsStr::new(pin)),
            &temporary.path().join("absent.sha256"),
            |_| panic!("pin skips checksum fetch"),
        )
        .unwrap();
        verify_runtime_archive(&archive, &expected).unwrap();
        std::fs::write(&archive, b"modified archive").unwrap();
        assert_eq!(
            verify_runtime_archive(&archive, &expected)
                .unwrap_err()
                .to_string(),
            Error::Command("Flame runtime checksum mismatch".into()).to_string()
        );
    }

    #[test]
    fn mirror_prefix_changes_archive_only_and_normalizes_trailing_slashes() {
        for (prefix, normalized) in [
            ("https://gh-proxy.com", "https://gh-proxy.com/"),
            ("https://gh-proxy.com/", "https://gh-proxy.com/"),
            ("https://ghproxy.net///", "https://ghproxy.net/"),
            (
                "https://mirror.example:8443/github/",
                "https://mirror.example:8443/github/",
            ),
            ("https://[::1]:8443/path/", "https://[::1]:8443/path/"),
        ] {
            let (archive, checksum) =
                runtime_download_urls(OFFICIAL_ARCHIVE, Some(prefix)).unwrap();
            assert_eq!(archive, format!("{normalized}{OFFICIAL_ARCHIVE}"));
            assert_eq!(checksum, format!("{OFFICIAL_ARCHIVE}.sha256"));
            assert!(!checksum.starts_with(normalized));
        }
    }

    #[test]
    fn invalid_mirror_configuration_is_rejected_without_exposing_it() {
        for prefix in [
            "",
            "mirror.example",
            "//mirror.example/",
            "http://mirror.example/",
            "https:///",
            "https://:443/",
            "https://user:private-password@mirror.example/",
            "https://mirror.example/?token=private-password",
            "https://mirror.example/#fragment",
            "https://mirror.example:invalid/",
            "https://mirror.example:0/",
            "https://mirror.example:65536/",
            "https://mirror.example/\nheader",
            "https://mirror.example/\\path",
        ] {
            let error = runtime_download_urls(OFFICIAL_ARCHIVE, Some(prefix)).unwrap_err();
            assert_eq!(error.to_string(), invalid_github_mirror().to_string());
            assert!(!error.to_string().contains("private-password"));
        }
    }

    #[test]
    fn upstream_installer_enables_systemd_in_the_selected_directory() {
        let settings = crate::Settings {
            worker: "worker-one".into(),
            namespace: "flame-system".into(),
            prefix: "/srv/flame space%$literal".into(),
            systemd_dir: "/test/systemd".into(),
            adopt_existing: false,
        };
        let args = installer_arguments(
            Path::new("/tmp/setup.sh"),
            &settings,
            "v0.8.0",
            "3.12",
            false,
        );
        assert_eq!(args[0], "FLAME_SYSTEMD_DIR=/test/systemd");
        assert_eq!(args[1], "FLAME_SYSTEMCTL=true");
        assert_eq!(args[2], "sh");
        assert!(args.iter().any(|arg| arg == "--join"));
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--no-systemd" || arg == "--force")
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--prefix", "/srv/flame space%$literal"])
        );
        let forced = installer_arguments(
            Path::new("/tmp/setup.sh"),
            &settings,
            "v0.8.0",
            "3.12",
            true,
        );
        assert!(forced.iter().any(|arg| arg == "--force"));
    }
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
