// the 2500 line budget, counted rather than estimated. tests do not ship and are
// not what has to be readable at 3am, so a file counts all but its test module

use std::fs;
use std::path::Path;

const BUDGET: usize = 2500;

// install.rs keeps remove() below mod tests, so stopping at #[cfg(test)] misses it
fn shipping(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| *l == "#[cfg(test)]") else {
        return lines.len();
    };
    let end = lines[start..]
        .iter()
        .position(|l| *l == "}")
        .map_or(lines.len(), |i| start + i + 1);
    start + lines.len() - end
}

#[test]
fn code_below_a_test_module_is_still_code() {
    let f = "one\ntwo\n#[cfg(test)]\nmod tests {\n    nope\n}\nthree\nfour\n";
    assert_eq!(shipping(f), 4);
}

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
        let n = shipping(&fs::read_to_string(f).unwrap());
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
