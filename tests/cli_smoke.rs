use std::process::Command;

#[test]
fn version_exits_before_process_lock() {
    let tmp = tempfile::TempDir::new().expect("create temp home");
    let fake_home_file = tmp.path().join("not-a-directory-home");
    std::fs::write(&fake_home_file, "not a directory").expect("create fake home file");

    let output = Command::new(env!("CARGO_BIN_EXE_getlatestrepo"))
        .arg("--version")
        .env("HOME", &fake_home_file)
        .output()
        .expect("run getlatestrepo --version");

    assert!(
        output.status.success(),
        "--version 应在获取进程锁前退出；stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("getlatestrepo {}", env!("CARGO_PKG_VERSION"))
    );
}
