use crate::tools::file_slice::compute_file_slice_result;
use crate::tools::schemas::file_slice::FileSliceRequest;
use tempfile::tempdir;

#[test]
fn cat_accepts_line_start_and_line_end_aliases() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();
    let root_display = root.to_string_lossy().to_string();

    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(
        root.join("src").join("main.rs"),
        "one\ntwo\nthree\nfour\nfive\n",
    )
    .expect("write file");

    let req: FileSliceRequest = serde_json::from_value(serde_json::json!({
        "file": "src/main.rs",
        "line_start": 2,
        "line_end": 4,
        "max_chars": 2000
    }))
    .expect("deserialize request");

    let out = compute_file_slice_result(root, &root_display, &req).expect("slice");
    assert_eq!(out.start_line, 2);
    assert_eq!(out.end_line, 4);
    assert_eq!(out.content, "two\nthree\nfour");
}

#[test]
fn cat_accepts_legacy_path_as_file_when_file_is_missing() {
    let tmp = tempdir().expect("tempdir");
    let root = tmp.path();
    let root_display = root.to_string_lossy().to_string();

    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(root.join("src").join("main.rs"), "alpha\nbeta\ngamma\n").expect("write file");

    let req: FileSliceRequest = serde_json::from_value(serde_json::json!({
        "path": "src/main.rs",
        "line_start": 1,
        "line_end": 2,
        "max_chars": 2000
    }))
    .expect("deserialize request");

    let out = compute_file_slice_result(root, &root_display, &req).expect("slice");
    assert_eq!(out.start_line, 1);
    assert_eq!(out.end_line, 2);
    assert_eq!(out.content, "alpha\nbeta");
}
