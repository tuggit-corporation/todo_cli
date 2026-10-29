use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn publish(existing: bool, fail: bool) -> (std::process::Output, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("bin")).unwrap();
    fs::create_dir(dir.path().join("dist")).unwrap();
    for asset in ["todo.tar.gz", "todo.tar.gz.sha256", "todo.rb"] {
        fs::write(dir.path().join("dist").join(asset), "fixture").unwrap();
    }
    let gh = dir.path().join("bin/gh");
    fs::write(
        &gh,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> requests
case "$1 $2" in
    "release view") [ "$MOCK_EXISTING" = true ] ;;
    "release create" | "release upload")
        if [ "$MOCK_FAIL" = true ]; then
            echo "publish failed" >&2
            exit 1
        fi
        ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(gh, fs::Permissions::from_mode(0o755)).unwrap();

    let workflow = include_str!("../.github/workflows/release.yml");
    let script = workflow
        .split_once("      - name: Publish GitHub release\n        run: |\n")
        .unwrap()
        .1
        .lines()
        .map_while(|line| line.strip_prefix("          "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!script.is_empty());
    let output = Command::new("bash")
        .args(["-e", "-c", &script])
        .current_dir(dir.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("TAG", "v0.1.0")
        .env("MOCK_EXISTING", existing.to_string())
        .env("MOCK_FAIL", fail.to_string())
        .output()
        .unwrap();
    let requests = fs::read_to_string(dir.path().join("requests")).unwrap();
    (output, requests)
}

#[test]
fn publish_creates_missing_release_with_all_assets() {
    let (output, requests) = publish(false, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests,
        "release view v0.1.0\nrelease create v0.1.0 dist/todo.rb dist/todo.tar.gz dist/todo.tar.gz.sha256 --verify-tag --title todo 0.1.0 --generate-notes\n"
    );
}

#[test]
fn publish_replaces_assets_on_existing_release() {
    let (output, requests) = publish(true, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        requests,
        "release view v0.1.0\nrelease upload v0.1.0 dist/todo.rb dist/todo.tar.gz dist/todo.tar.gz.sha256 --clobber\n"
    );
}

#[test]
fn publish_propagates_creation_and_upload_failures() {
    for existing in [false, true] {
        let (output, _) = publish(existing, true);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("publish failed"));
    }
}
