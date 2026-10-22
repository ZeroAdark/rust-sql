include!("../build.rs");

#[test]
fn production_staging_excludes_test_modules_and_cleans_regression_files() {
    let root = std::env::temp_dir().join(format!("extension-staging-{}", std::process::id()));
    let staged = root.join("extension");
    let tsdata = root.join("tsearch_data");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::create_dir_all(&tsdata).unwrap();
    let contrib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../contrib");
    for enabled in [false, true, false] {
        stage_contrib(&contrib, &staged, &tsdata, enabled);
        for name in ["injection_points", "test_custom_types"] {
            for suffix in [".control", "--1.0.sql"] {
                assert_eq!(staged.join(format!("{name}{suffix}")).exists(), enabled);
            }
        }
        assert!(staged.join("hstore.control").is_file());
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn staged_share_carries_c_share_files() {
    let share = std::env::temp_dir().join(format!("share-staging-{}", std::process::id()));
    stage_share(Path::new(env!("CARGO_MANIFEST_DIR")), &share, false);
    let mut want = vec![
        "postgres.bki".to_string(),
        "extension/plpgsql.control".to_string(),
        "extension/plpgsql--1.0.sql".to_string(),
        "extension/intagg.control".to_string(),
        "extension/intagg--1.1.sql".to_string(),
        "extension/intagg--1.0--1.1.sql".to_string(),
    ];
    // src/backend/snowball/stopwords, installed into tsearch_data by C.
    for lang in [
        "danish", "dutch", "english", "finnish", "french", "german", "hungarian", "italian",
        "nepali", "norwegian", "portuguese", "russian", "spanish", "swedish", "turkish",
    ] {
        want.push(format!("tsearch_data/{lang}.stop"));
    }
    // src/timezone/data/tzdata.zi compiled by C's zic, installed into timezone.
    for zone in ["UTC", "GMT", "Europe/Dublin", "Eire", "America/New_York", "Etc/GMT+5"] {
        want.push(format!("timezone/{zone}"));
    }
    let missing: Vec<_> = want.iter().filter(|f| !share.join(f).is_file()).collect();
    let tz_files = count_files(&share.join("timezone"));
    let posixrules = share.join("timezone/posixrules").exists();
    std::fs::remove_dir_all(&share).unwrap();
    assert!(missing.is_empty(), "staged share tree lacks {missing:?}");
    assert_eq!(tz_files, 598, "C 18.6 installs 598 zone files");
    assert!(!posixrules, "C's zic no longer installs posixrules");
}

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.map(|e| e.unwrap().path())
                .map(|p| if p.is_dir() { count_files(&p) } else { 1 })
                .sum()
        })
        .unwrap_or(0)
}
