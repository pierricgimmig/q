const TEMPLATE_SECTIONS: &[&str] = &[
    "Goal",
    "Repository / target",
    "Scope",
    "Deliverable",
    "Acceptance criteria",
    "Constraints / do not do",
    "Dependencies",
];

/// A Markdown skeleton with one empty heading per template section. Used to
/// seed the editor for `q add --edit`; none of the sections are required.
pub fn body_template() -> String {
    let mut out = String::new();
    for label in TEMPLATE_SECTIONS {
        out.push_str("## ");
        out.push_str(label);
        out.push_str("\n\n");
    }
    out
}

pub fn acceptance_criteria(body: Option<&str>) -> Vec<String> {
    let body = match body {
        Some(body) => body,
        None => return Vec::new(),
    };
    let mut collecting = false;
    let mut items = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(heading) = heading_text(trimmed) {
            if collecting {
                break;
            }
            collecting = heading.contains("acceptance");
            continue;
        }
        if !collecting {
            continue;
        }
        if let Some(item) = list_item(trimmed) {
            if !item.is_empty() {
                items.push(item);
            }
        }
    }
    items
}

fn heading_text(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix('#')?;
    let rest = rest.trim_start_matches('#').trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_ascii_lowercase())
    }
}

fn list_item(trimmed: &str) -> Option<String> {
    if let Some(rest) = trimmed.strip_prefix("- ") {
        return Some(rest.trim().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("* ") {
        return Some(rest.trim().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("+ ") {
        return Some(rest.trim().to_string());
    }
    let bytes = trimmed.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx > 0 && trimmed[idx..].starts_with(". ") {
        return Some(trimmed[idx + 2..].trim().to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
## Goal
Ship a benchmark

## Repository / target
github.com/acme/profiler-core

## Scope
Harness only

## Deliverable
A markdown report

## Acceptance criteria
- Compare baseline, varint, and delta-coded timestamp streams
- Do not change the production storage format

## Constraints / do not do
- No production format changes

## Dependencies
- None
"#;

    #[test]
    fn full_body_parses_criteria() {
        assert_eq!(
            acceptance_criteria(Some(FULL)),
            vec![
                "Compare baseline, varint, and delta-coded timestamp streams".to_string(),
                "Do not change the production storage format".to_string(),
            ]
        );
    }

    #[test]
    fn sparse_body_has_no_criteria() {
        assert!(acceptance_criteria(Some("just an idea")).is_empty());
        assert!(acceptance_criteria(None).is_empty());
    }

    #[test]
    fn body_template_has_one_heading_per_section() {
        let template = body_template();
        for label in TEMPLATE_SECTIONS {
            assert!(template.contains(&format!("## {label}\n\n")), "{label}");
        }
    }
}
