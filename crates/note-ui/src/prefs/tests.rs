use super::*;

/// A scratch preference path, removed first so a test always starts from nothing.
fn scratch(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("cheap-note-prefs-{name}.json"));
    let _ = std::fs::remove_file(&path);
    path
}

/// A file that is not there is the shipped step, not an error.
#[test]
fn a_missing_file_is_the_shipped_scale() {
    let path = scratch("missing");
    let prefs = Prefs::load_from(&path);

    assert_eq!(prefs.scale(), ScaleStep::DEFAULT);
    assert!(!path.exists(), "reading must not create the file");
}

/// The step is stored as its index, and every step the interface has makes the round trip.
#[test]
fn the_scale_index_survives_a_round_trip() {
    let path = scratch("round-trip");

    for step in ScaleStep::ALL {
        let mut prefs = Prefs::load_from(&path);
        assert!(prefs.set_scale(step), "a new step is a change");
        assert!(!prefs.set_scale(step), "the same step is not a change");
        prefs.save().expect("the preference written");

        let text = std::fs::read_to_string(&path).expect("the file is there");
        let json: Value = serde_json::from_str(&text).expect("the file is JSON");

        assert_eq!(
            json["interface_scale"].as_u64(),
            Some(step.index() as u64),
            "the file holds the index, not a percentage"
        );
        assert_eq!(Prefs::load_from(&path).scale(), step);
    }

    let _ = std::fs::remove_file(&path);
}

/// A missing field is the shipped step, not the first index in the list.
#[test]
fn an_empty_object_is_the_shipped_scale() {
    let path = scratch("empty-object");
    std::fs::write(&path, "{}").expect("a preference file");

    assert_eq!(Prefs::load_from(&path).scale(), ScaleStep::DEFAULT);

    let _ = std::fs::remove_file(&path);
}

/// An index this build does not know, or a value that is not an index at all, falls back rather
/// than refusing to open.
#[test]
fn an_unknown_index_falls_back_to_the_shipped_scale() {
    let path = scratch("unknown");

    for text in [
        r#"{"interface_scale": 99}"#,
        r#"{"interface_scale": 1.5}"#,
        r#"{"interface_scale": "150%"}"#,
        r#"{"interface_scale": null}"#,
    ] {
        std::fs::write(&path, text).expect("a preference file");
        assert_eq!(
            Prefs::load_from(&path).scale(),
            ScaleStep::DEFAULT,
            "{text} named no step this build has"
        );
    }

    let _ = std::fs::remove_file(&path);
}

/// A file that is not JSON is the shipped step too: the interface still opens.
#[test]
fn a_broken_file_is_the_shipped_scale() {
    let path = scratch("broken");
    std::fs::write(&path, "not a preference").expect("a broken preference file");

    assert_eq!(Prefs::load_from(&path).scale(), ScaleStep::DEFAULT);

    let _ = std::fs::remove_file(&path);
}

/// A field a newer build wrote is kept when this build saves the one field it knows.
#[test]
fn an_unknown_field_is_not_lost_when_the_scale_is_saved() {
    let path = scratch("unknown-field");
    std::fs::write(
        &path,
        r#"{"interface_scale": 1, "future": {"colour": "red"}}"#,
    )
    .expect("a preference file");

    let mut prefs = Prefs::load_from(&path);
    prefs.set_scale(ScaleStep::P150);
    prefs.save().expect("the preference written");

    let text = std::fs::read_to_string(&path).expect("the file is there");
    let json: Value = serde_json::from_str(&text).expect("the file is JSON");

    assert_eq!(json["future"]["colour"], "red");
    assert_eq!(
        json["interface_scale"].as_u64(),
        Some(ScaleStep::P150.index() as u64)
    );

    let _ = std::fs::remove_file(&path);
}
