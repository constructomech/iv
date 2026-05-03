use std::fs;
use std::path::PathBuf;

const UI_FILES: &[&str] = &[
    "src/folder_tree.rs",
    "src/grid_view.rs",
    "src/image_view.rs",
    "src/main.rs",
];

const UI_APIS: &[&str] = &[
    ".hint_text(",
    ".on_hover_text(",
    ".selected_text(",
    "Button::new(",
    "RichText::new(",
    "ui.label(",
    "painter.text(",
    "selectable_value(",
];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();

    for rel in UI_FILES {
        let path = root.join(rel);
        let source = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", path.display());
        });
        lint_file(rel, &source, &mut failures);
    }

    if !failures.is_empty() {
        eprintln!("hard-coded UI strings found; use rust_i18n::t!(...) or add an allow comment:");
        for failure in failures {
            eprintln!("  {failure}");
        }
        std::process::exit(1);
    }
}

fn lint_file(path: &str, source: &str, failures: &mut Vec<String>) {
    for (line_idx, line) in source.lines().enumerate() {
        if line.contains("i18n-allow") || line.contains("rust_i18n::t!") || line.contains("t!(") {
            continue;
        }
        if !UI_APIS.iter().any(|api| line.contains(api)) {
            continue;
        }
        if has_string_literal(line) {
            failures.push(format!("{}:{}: {}", path, line_idx + 1, line.trim()));
        }
    }
}

fn has_string_literal(line: &str) -> bool {
    let mut escaped = false;
    let mut in_char = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '\'' {
            in_char = !in_char;
            continue;
        }
        if ch != '"' || in_char {
            continue;
        }
        let prefix = line[..idx].chars().rev().find(|c| !c.is_whitespace());
        if prefix != Some('!') {
            return true;
        }
    }
    false
}
