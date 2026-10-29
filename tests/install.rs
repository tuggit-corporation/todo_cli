use std::{fs, os::unix::fs::PermissionsExt, process::Command};

struct Installer {
    dir: tempfile::TempDir,
}

impl Installer {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        fs::write(dir.path().join("todo"), "#!/bin/sh\necho fixture\n").unwrap();
        let status = Command::new("tar")
            .current_dir(dir.path())
            .args(["-czf", "release.tar.gz", "todo"])
            .status()
            .unwrap();
        assert!(status.success());
        let checksum = Command::new("shasum")
            .args(["-a", "256"])
            .arg(dir.path().join("release.tar.gz"))
            .output()
            .unwrap();
        assert!(checksum.status.success());
        fs::write(dir.path().join("checksum"), checksum.stdout).unwrap();
        for (name, script) in [
            (
                "uname",
                "#!/bin/sh\ncase \"$1\" in -s) echo \"$MOCK_OS\";; -m) echo \"$MOCK_ARCH\";; esac\n",
            ),
            (
                "curl",
                r#"#!/bin/sh
set -eu
out=
url=
latest=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) out=$2; shift ;;
        -w) latest=true; shift ;;
        https:*) url=$1 ;;
    esac
    shift
done
if [ "$latest" = true ]; then
    printf '%s' "$MOCK_LATEST"
    exit 0
fi
printf '%s\n' "$url" >> "$MOCK_DIR/requests"
case "$url" in
    *.sha256) cp "$MOCK_DIR/checksum" "$out" ;;
    *.tar.gz) cp "$MOCK_DIR/release.tar.gz" "$out" ;;
    *) exit 22 ;;
esac
"#,
            ),
        ] {
            let path = dir.path().join("bin").join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn run(&self, os: &str, arch: &str, latest: &str) -> std::process::Output {
        Command::new("sh")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("MOCK_DIR", self.dir.path())
            .env("MOCK_OS", os)
            .env("MOCK_ARCH", arch)
            .env("MOCK_LATEST", latest)
            .env("TODO_INSTALL_DIR", self.dir.path().join("installed bin"))
            .output()
            .unwrap()
    }
}

const RELEASE: &str = "https://github.com/tuggit-corporation/todo_cli/releases/tag/v0.1.0";

#[test]
fn installer_selects_release_for_each_supported_platform_and_verifies_download() {
    for (os, arch, target) in [
        ("Darwin", "arm64", "aarch64-apple-darwin"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ("Linux", "aarch64", "aarch64-unknown-linux-gnu"),
        ("Linux", "x86_64", "x86_64-unknown-linux-gnu"),
    ] {
        let installer = Installer::new();
        let result = installer.run(os, arch, RELEASE);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let binary = installer.dir.path().join("installed bin/todo");
        assert_eq!(
            fs::read_to_string(&binary).unwrap(),
            "#!/bin/sh\necho fixture\n"
        );
        assert_ne!(
            fs::metadata(binary).unwrap().permissions().mode() & 0o111,
            0
        );
        let requests = fs::read_to_string(installer.dir.path().join("requests")).unwrap();
        assert!(requests.contains(&format!("todo-0.1.0-{target}.tar.gz\n")));
        assert!(requests.contains(&format!("todo-0.1.0-{target}.tar.gz.sha256\n")));
    }
}

#[test]
fn installer_preserves_existing_binary_on_checksum_failure() {
    let installer = Installer::new();
    let directory = installer.dir.path().join("installed bin");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("todo"), "existing binary").unwrap();
    fs::write(
        installer.dir.path().join("checksum"),
        format!("{}  release.tar.gz\n", "0".repeat(64)),
    )
    .unwrap();
    let result = installer.run("Linux", "x86_64", RELEASE);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Checksum mismatch"));
    assert_eq!(
        fs::read_to_string(directory.join("todo")).unwrap(),
        "existing binary"
    );
}

#[test]
fn installer_rejects_missing_releases_and_unsupported_platforms() {
    let installer = Installer::new();
    for (os, arch, release) in [
        (
            "Darwin",
            "arm64",
            "https://github.com/tuggit-corporation/todo_cli/releases",
        ),
        ("Windows", "x86_64", RELEASE),
        ("Linux", "i686", RELEASE),
    ] {
        assert!(!installer.run(os, arch, release).status.success());
    }
    assert!(!installer.dir.path().join("installed bin/todo").exists());
    assert!(!installer.dir.path().join("requests").exists());
}
