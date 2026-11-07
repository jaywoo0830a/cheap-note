use super::*;

#[test]
fn timestamps_are_iso_utc() {
    assert_eq!(format_timestamp(0, 0), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        format_timestamp(1_700_000_000, 123),
        "2023-11-14T22:13:20.123Z"
    );
    assert_eq!(format_timestamp(951_782_400, 0), "2000-02-29T00:00:00.000Z");
}

#[test]
fn a_log_writes_a_timestamped_line() {
    let path =
        std::env::temp_dir().join(format!("cheap-note-status-log-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let mut log = StatusLog::open_at(path.clone());
    assert!(log.is_on());
    log.write("ink 1.2 ms");

    let text = std::fs::read_to_string(&path).expect("the log");
    assert!(text.contains("# cheap-note status log"));
    assert!(text.lines().any(|line| line.ends_with("  ink 1.2 ms")));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_path_is_beside_the_notes() {
    let log = path();
    assert_eq!(log.file_name(), Some(std::ffi::OsStr::new(FILE)));
    assert_eq!(log.parent(), note::root().parent());
}
