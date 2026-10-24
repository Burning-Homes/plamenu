#[path = "../benches/hot_paths/read_selection.rs"]
mod read_selection;

use read_selection::ReadSelection;

#[test]
fn full_inventory_keeps_all_cases_and_existing_criterion_arguments() {
    let selection = ReadSelection::new(None, &["api/home".into(), "--exact".into()]).unwrap();
    assert_eq!(selection.case(), None);
    assert_eq!(selection.criterion_arguments(), ["api/home", "--exact"]);
    for (group, name) in [("api", "home"), ("web", "home"), ("ap", "actor")] {
        assert!(selection.includes(group, name));
    }
    selection.validate_ids([]).unwrap();
}

#[test]
fn focused_id_is_exact_in_both_group_and_name() {
    let selection = ReadSelection::new(
        Some("api/home_sparse".into()),
        &["--bench".into(), "--noplot".into()],
    )
    .unwrap();
    assert_eq!(selection.case(), Some("api/home_sparse"));
    assert!(selection.includes("api", "home_sparse"));
    assert!(!selection.includes("api", "home_sparse_extra"));
    assert!(!selection.includes("web", "home_sparse"));
    selection.validate_ids([("api", "home_sparse")]).unwrap();
    assert!(selection.validate_ids([("api", "home_dense")]).is_err());
}

#[test]
fn independent_criterion_filters_and_modes_cannot_change_the_plan() {
    for arg in [
        "api/home",
        "--exact",
        "--ignored",
        "--test",
        "--list",
        "--measurement-time",
        "0.1",
    ] {
        assert!(ReadSelection::new(Some("api/home_sparse".into()), &[arg.into()]).is_err());
    }
}

#[test]
fn malformed_or_non_read_ids_fail_instead_of_measuring_nothing() {
    for id in [
        "",
        "api/",
        "api/home.*",
        "api/home/extra",
        "api/home\n",
        "write/post_status",
    ] {
        assert!(ReadSelection::new(Some(id.into()), &[]).is_err());
    }
    for id in ["api/home_sparse", "web/web_home", "ap/actor"] {
        assert!(ReadSelection::new(Some(id.into()), &[]).is_ok());
    }
}
