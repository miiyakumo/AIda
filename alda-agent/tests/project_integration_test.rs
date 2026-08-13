use alda_agent::alda::{AldaCheck, CheckStatus};
use alda_agent::config::ModelConfig;
use alda_agent::project::Project;

fn checks() -> Vec<AldaCheck> {
    vec![AldaCheck {
        name: "Alda 语法",
        status: CheckStatus::Pass,
        detail: "解析成功".to_string(),
    }]
}

#[test]
fn project_survives_restart_restore_and_new_edit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("music-project");
    let mut project = Project::load_or_create(root.clone(), "music-project", "source").unwrap();
    project.add_user_message("完整器乐曲").unwrap();
    assert_eq!(
        project
            .save_version("piano: c", "首次创作", &checks())
            .unwrap(),
        1
    );
    assert_eq!(
        project
            .save_version("piano: d", "局部修改", &checks())
            .unwrap(),
        2
    );
    project.restore_version(1).unwrap();
    drop(project);

    let mut restarted = Project::load_or_create(root, "ignored", "ignored").unwrap();
    assert_eq!(restarted.current_version(), 1);
    assert_eq!(restarted.versions().len(), 2);
    assert_eq!(restarted.conversation().first_request(), Some("完整器乐曲"));
    assert_eq!(
        restarted
            .save_version("piano: e", "恢复后修改", &checks())
            .unwrap(),
        3
    );
    assert_eq!(restarted.version_code(2).unwrap(), "piano: d");
}

#[test]
fn model_configuration_persists_without_leaking_into_project_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("music-project");
    let mut project = Project::load_or_create(root.clone(), "music-project", "source").unwrap();
    project
        .save_version("piano: c", "首次创作", &checks())
        .unwrap();
    let mut model = ModelConfig::default();
    model.set_model("example-model").unwrap();
    model.set_base_url("https://api.example.com").unwrap();
    model.set_api_key("secret-test-value").unwrap();
    model.save(&root).unwrap();
    drop(project);

    let resolved = ModelConfig::load(&root).unwrap().resolve().unwrap();
    assert_eq!(resolved.model, "example-model");
    assert_eq!(resolved.base_url, "https://api.example.com");
    assert_eq!(resolved.api_key, "secret-test-value");

    for entry in [
        root.join("project.json"),
        root.join("current.alda"),
        root.join("versions/0001.alda"),
    ] {
        let content = std::fs::read_to_string(entry).unwrap();
        assert!(!content.contains("secret-test-value"));
    }
}
