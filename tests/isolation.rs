//! Only the simulation and the evaluation may read the truth.

#[test]
fn engine_code_never_reads_the_truth() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                if p.file_name().unwrap() != "sim" {
                    stack.push(p);
                }
                continue;
            }
            let rel = p.strip_prefix(&root).unwrap().to_string_lossy().to_string();
            if ["eval.rs", "main.rs", "lib.rs", "report.rs"].contains(&rel.as_str()) {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            for banned in ["sim::", "truth", "WorldTruth", "DepositTruth"] {
                assert!(!src.contains(banned), "{rel} mentions {banned:?}");
            }
            checked += 1;
        }
    }
    assert!(checked >= 15, "checked {checked} files");
}
