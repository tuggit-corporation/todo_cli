use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Default, Deserialize, Serialize)]
struct Config {
    update_repo: Option<String>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|s| {
            s.is_empty()
                || *s == "."
                || *s == ".."
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        bail!("Release repository must be OWNER/REPO, not a URL");
    }
    Ok(())
}

pub fn configured_repo(path: &Path) -> Result<Option<String>> {
    let repo = match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice::<Config>(&bytes)
                .context("Invalid todo configuration")?
                .update_repo
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    Ok(repo.or_else(|| option_env!("TODO_RELEASE_REPO").map(str::to_owned)))
}

pub fn save_repo(path: &Path, repo: &str) -> Result<()> {
    validate_repo(repo)?;
    let parent = path
        .parent()
        .context("Configuration has no parent directory")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(
        &mut file,
        &Config {
            update_repo: Some(repo.into()),
        },
    )?;
    file.as_file().sync_all()?;
    file.persist(path)
        .context("Cannot save update repository")?;
    Ok(())
}

fn client(timeout: u64) -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("todo/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(timeout))
        .build()?)
}

fn newer_version(tag: &str, current: &str) -> Result<Option<Version>> {
    let latest = Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .context("Release tag must be a semantic version, such as v0.2.0")?;
    let current = Version::parse(current)?;
    Ok((latest > current && latest.pre.is_empty()).then_some(latest))
}

fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        _ => bail!("Self-update is not supported on this platform; install from source"),
    }
}

fn homebrew_install(path: &Path) -> bool {
    let parts: Vec<_> = path.components().collect();
    parts
        .windows(2)
        .any(|p| p[0].as_os_str() == "Cellar" && p[1].as_os_str() == "todo")
}

pub fn run(repo: &str, check_only: bool, yes: bool, json: bool) -> Result<()> {
    validate_repo(repo)?;
    let release: Release = client(4)?
        .get(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()
        .context("Cannot read GitHub Releases (a published, public release is required)")?
        .json()?;
    let version = newer_version(&release.tag_name, env!("CARGO_PKG_VERSION"))?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "current_version": env!("CARGO_PKG_VERSION"),
                "latest_version": release.tag_name,
                "update_available": version.is_some(),
                "repository": repo,
            })
        );
        return Ok(());
    }
    let Some(version) = version else {
        if check_only {
            println!("todo {} is up to date.", env!("CARGO_PKG_VERSION"));
        }
        return Ok(());
    };
    eprintln!(
        "todo {version} is available (installed: {}).",
        env!("CARGO_PKG_VERSION")
    );
    if homebrew_install(&std::env::current_exe()?.canonicalize()?) {
        eprintln!("Managed by Homebrew. Run: brew update && brew upgrade todo");
        return Ok(());
    }
    if check_only {
        return Ok(());
    }
    if !yes {
        if !io::stdin().is_terminal() {
            bail!("Update requires confirmation; use --yes to install or --check to only check");
        }
        eprint!("Download and install from github.com/{repo}? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Ok(());
        }
    }
    let name = format!("todo-{version}-{}.tar.gz", target()?);
    for required in [&name, &format!("{name}.sha256")] {
        if !release.assets.iter().any(|a| &a.name == required) {
            bail!(
                "Release is missing asset {required}; install manually or contact the maintainer"
            );
        }
    }
    let base = format!(
        "https://github.com/{repo}/releases/download/{}",
        release.tag_name
    );
    let client = client(120)?;
    let checksum = download(&client, &format!("{base}/{name}.sha256"), 1024)?;
    let checksum = std::str::from_utf8(&checksum)?
        .split_whitespace()
        .next()
        .unwrap_or("");
    if checksum.len() != 64 || !checksum.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Release checksum is malformed");
    }
    let archive = download(&client, &format!("{base}/{name}"), 50 * 1024 * 1024)?;
    verify_checksum(&archive, checksum)?;
    let executable = std::env::current_exe()?.canonicalize()?;
    install(&archive, &executable)?;
    eprintln!("Installed todo {version}. Restart todo to use the new version.");
    Ok(())
}

fn download(client: &Client, url: &str, limit: u64) -> Result<Vec<u8>> {
    let response = client.get(url).send()?.error_for_status()?;
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        bail!("Release download exceeds size limit");
    }
    Ok(bytes)
}

fn verify_checksum(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("Release checksum does not match; the executable was not changed");
    }
    Ok(())
}

fn install(archive: &[u8], executable: &Path) -> Result<()> {
    let parent: PathBuf = executable
        .parent()
        .context("Executable has no parent directory")?
        .into();
    let mut replacement = tempfile::NamedTempFile::new_in(&parent)
        .context("Cannot write to the executable directory; use a user-writable installation")?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut found = false;
    for entry in tar.entries()? {
        let entry = entry?;
        if entry.path()?.as_ref() == Path::new("todo") && entry.header().entry_type().is_file() {
            if found {
                bail!("Release contains duplicate executables");
            }
            let size = io::copy(&mut entry.take(100 * 1024 * 1024 + 1), &mut replacement)?;
            if size == 0 || size > 100 * 1024 * 1024 {
                bail!("Release executable has invalid size");
            }
            found = true;
        }
    }
    if !found {
        bail!("Release archive does not contain a todo executable");
    }
    fs::set_permissions(replacement.path(), fs::metadata(executable)?.permissions())?;
    replacement.as_file().sync_all()?;
    replacement
        .persist(executable)
        .context("Cannot atomically replace the todo executable")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_versions_and_repository_input() {
        assert!(newer_version("v0.2.0", "0.1.0").unwrap().is_some());
        assert!(newer_version("v0.1.0", "0.1.0").unwrap().is_none());
        assert!(newer_version("v0.0.9", "0.1.0").unwrap().is_none());
        assert!(newer_version("v0.2.0-rc.1", "0.1.0").unwrap().is_none());
        assert!(newer_version("garbage", "0.1.0").is_err());
        for bad in [
            "https://github.com/a/b",
            "a",
            "a/b/c",
            "../b",
            "a/b?x=1",
            "/",
        ] {
            assert!(validate_repo(bad).is_err(), "{bad}");
        }
        validate_repo("my-org/todo").unwrap();
    }

    fn archive(binary: &[u8]) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(binary.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "todo", binary).unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn checksum_failure_and_invalid_archives_preserve_executable() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("todo");
        fs::write(&executable, b"old").unwrap();
        assert!(verify_checksum(b"data", &"0".repeat(64)).is_err());
        assert!(install(b"invalid archive", &executable).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"old");
        assert!(install(&archive(b""), &executable).is_err());
        assert_eq!(fs::read(&executable).unwrap(), b"old");
        let bytes = archive(b"new");
        verify_checksum(&bytes, &format!("{:x}", Sha256::digest(&bytes))).unwrap();
        install(&bytes, &executable).unwrap();
        assert_eq!(fs::read(&executable).unwrap(), b"new");
    }

    #[test]
    fn repository_configuration_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/config.json");
        save_repo(&path, "owner/todo").unwrap();
        assert_eq!(
            configured_repo(&path).unwrap().as_deref(),
            Some("owner/todo")
        );
        assert!(save_repo(&path, "not a repo").is_err());
        assert_eq!(
            configured_repo(&path).unwrap().as_deref(),
            Some("owner/todo")
        );
    }

    #[test]
    fn recognizes_homebrew_without_matching_unrelated_paths() {
        assert!(homebrew_install(Path::new(
            "/opt/homebrew/Cellar/todo/0.1.0/bin/todo"
        )));
        assert!(homebrew_install(Path::new(
            "/home/linuxbrew/.linuxbrew/Cellar/todo/0.1.0/bin/todo"
        )));
        assert!(!homebrew_install(Path::new("/home/me/.cargo/bin/todo")));
        assert!(!homebrew_install(Path::new(
            "/home/me/Cellar/another/bin/todo"
        )));
    }
}
