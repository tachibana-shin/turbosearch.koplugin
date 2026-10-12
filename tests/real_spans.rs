//! Re-runs the span boundary invariant against real book text, where the unit
//! tests' synthetic inputs did not reproduce the failure.

#[test]
#[ignore = "needs a reference library: set TURBOSEARCH_LIBRARY"]
fn real_book_slices_are_always_on_char_boundaries() {
    let Some(root) = turbosearch::library::root() else {
        eprintln!("reference library not set; skipping");
        return;
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        eprintln!("reference library unreadable; skipping");
        return;
    };
    let mut checked = 0;
    let mut failures: Vec<(String, usize, usize, String)> = Vec::new();

    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("epub") {
            continue;
        }
        let Ok(content) = turbosearch::format::epub::open(&p) else {
            continue;
        };
        checked += 1;
        for section in &content.sections {
            let text = String::from_utf8_lossy(&section.text);
            turbosearch::tokenize::for_each_token_spanned(&text, 0, |_, _, s, e| {
                if !text.is_char_boundary(s) || !text.is_char_boundary(e) || s > e {
                    failures.push((
                        p.file_name().unwrap().to_string_lossy().into_owned(),
                        s,
                        e,
                        text[s.min(e)..].chars().take(3).collect(),
                    ));
                }
            });
        }
        if checked >= 40 {
            break;
        }
    }
    eprintln!("books checked: {checked}, failures: {}", failures.len());
    for f in failures.iter().take(5) {
        eprintln!("  {f:?}");
    }
    assert!(
        failures.is_empty(),
        "{} span(s) are not usable",
        failures.len()
    );
}
