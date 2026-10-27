//! Status text may only claim what the code can back up.
//!
//! This scans every string literal in the client's non-test source for claims
//! Relay cannot currently support: data kept on two hosts, automatic
//! continuation on another host, guaranteed or lossless quality. A feature that
//! later earns such a claim must change this list in the same commit, next to
//! the evidence that backs it.


const UNBACKED: [&str; 14] = [
    "secured on two hosts",
    "saved on two hosts",
    "stored on two hosts",
    "kept on two hosts",
    "backed up on two",
    "safe on two hosts",
    "continues on another host",
    "switches hosts automatically",
    "automatic failover",
    "guaranteed",
    "lossless",
    "crystal clear",
    "zero latency",
    "no lag",
];

/// String literal pieces on one line: the text between quote pairs.
fn literals(line: &str) -> impl Iterator<Item = &str> {
    line.split('"').skip(1).step_by(2)
}

fn offending(source: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    for (number, line) in lines.iter().enumerate() {
        // Stop at the test module; a single test-only function is still scanned.
        if line.trim_start().starts_with("#[cfg(test)]")
            && lines
                .get(number + 1)
                .is_some_and(|next| next.trim_start().starts_with("mod "))
        {
            break;
        }
        if line.trim_start().starts_with("//") {
            continue;
        }
        for literal in literals(line) {
            let lower = literal.to_lowercase();
            for claim in UNBACKED {
                if lower.contains(claim) {
                    found.push((number + 1, claim));
                }
            }
        }
    }
    found
}

#[test]
fn the_scanner_finds_a_claim_and_ignores_tests_and_comments() {
    let sample = "let a = \"Your server is secured on two hosts\";\n// \"guaranteed\" in a comment\n#[cfg(test)]\nfn f() {}\nlet c = \"no lag\";\n#[cfg(test)]\nmod tests {\nlet b = \"guaranteed\";\n";
    assert_eq!(offending(sample), vec![(1, "secured on two hosts"), (5, "no lag")]);
}

#[test]
fn no_status_text_makes_an_unbacked_claim() {
    let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut problems = Vec::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&folder).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs")
            || path.file_name().and_then(|n| n.to_str()) == Some("honest_claims.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        scanned += 1;
        for (line, claim) in offending(&text) {
            problems.push(format!("{}:{line}: {claim}", path.display()));
        }
    }
    assert!(scanned > 100, "the scan must cover the client source");
    assert!(problems.is_empty(), "{problems:#?}");
}
