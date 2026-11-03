//! `migrate_legacy` against the old plugin's config dir layout. The fixture is
//! the old `config.default.yaml`, byte-identical to the owner's live
//! `shadowfax.comments/config.yaml` (that file is never read by tests).

use tempfile::tempdir;
use tools_comment::migrate_legacy;
use tools_core::Config;

const OLD_CONFIG: &str = include_str!("fixtures/config.default.yaml");

fn legacy_root(config: &str) -> tempfile::TempDir {
    let root = tempdir().unwrap();
    let dir = root.path().join("shadowfax.comments");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("config.yaml"), config).unwrap();
    root
}

#[test]
fn the_owners_config_becomes_comment_and_popup_tables() {
    let root = legacy_root(OLD_CONFIG);

    let fragment = migrate_legacy(root.path()).unwrap();

    let expected: toml::Table = r#"
[comment]
inline_comments = true

[popup.size]
comment-capture = { width = "90%", height = "90%" }
comment-review = { width = "90%", height = "85%" }

[[popup.profile]]
name = "laptop"
max_client_width = 310
size.comment-capture = { width = "95%", height = "90%" }
size.comment-review = { width = "95%", height = "85%" }

[[popup.profile]]
name = "2-3-ultrawide"
max_client_width = 350
size.comment-capture = { width = "90%", height = "90%" }
size.comment-review = { width = "90%", height = "85%" }

[[popup.profile]]
name = "full-ultrawide"
min_client_width = 400
size.comment-capture = { width = "50%", height = "70%" }
size.comment-review = { width = "70%", height = "85%" }
"#
    .parse()
    .unwrap();
    assert_eq!(fragment, expected);
    Config::parse(&toml::to_string(&fragment).unwrap()).unwrap();
}

/// Core skips a matching profile that does not size a key; the old plugin fell
/// back to the base size instead. Filling every profile keeps the old answer.
#[test]
fn profiles_that_sized_one_popup_fall_back_to_the_base_size_for_the_other() {
    let root = legacy_root(
        r#"
popups:
  capture: { width: "90%", height: "90%" }
  review: { width: "80", height: "30" }
profiles:
  - name: narrow
    match: { max_client_width: 200 }
    popups:
      capture: { width: "99%", height: "99%" }
  - name: everything
    match: {}
    popups:
      review: { width: "50%", height: "50%" }
"#,
    );

    let text = toml::to_string(&migrate_legacy(root.path()).unwrap()).unwrap();
    let config = Config::parse(&text).unwrap();

    let review = config.popup("comment-review", Some(150)).unwrap();
    assert_eq!(review.profile.as_deref(), Some("narrow"), "{text}");
    assert_eq!(review.dims.width.to_string(), "80");
    assert_eq!(review.dims.height.to_string(), "30");
    assert_eq!(
        config
            .popup("comment-capture", Some(150))
            .unwrap()
            .dims
            .width
            .to_string(),
        "99%"
    );
    assert!(
        !config.section_table("comment").unwrap().unwrap()["inline_comments"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn no_old_config_is_an_empty_fragment_and_a_broken_one_is_an_error() {
    let empty = tempdir().unwrap();
    assert!(migrate_legacy(empty.path()).unwrap().is_empty());

    let broken = legacy_root("popups:\n  capture: { width: \"90%\", height: \"90%\" }\n");
    let error = migrate_legacy(broken.path()).unwrap_err();
    assert!(
        format!("{error:#}").contains("popup `review` is required"),
        "{error:#}"
    );
}
