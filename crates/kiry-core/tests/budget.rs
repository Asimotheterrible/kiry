// the 2500 line budget, counted rather than estimated. tests do not ship and are
// not what has to be readable at 3am, so a file counts up to its test module

use std::fs;
use std::path::Path;

const BUDGET: usize = 2500;

#[test]
fn kiry_core_fits_in_its_budget() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = fs::read_dir(&src)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    files.sort();

    let mut total = 0;
    let mut rows = String::new();
    for f in &files {
        let text = fs::read_to_string(f).unwrap();
        let n = text.lines().take_while(|l| *l != "#[cfg(test)]").count();
        total += n;
        rows.push_str(&format!(
            "\n  {n:>5}  {}",
            f.file_name().unwrap_or_default().to_string_lossy()
        ));
    }

    assert!(
        total <= BUDGET,
        "kiry-core is {total} lines against a budget of {BUDGET}. \
         delete something or move it into the kiry crate, do not raise the number.{rows}"
    );
    println!("kiry-core {total}/{BUDGET}{rows}");
}
