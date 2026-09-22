const RECOMMENDED: &[(&str, &str)] = &[
    ("Goal", "goal"),
    ("Repository / target", "repository"),
    ("Scope", "scope"),
    ("Deliverable", "deliverable"),
    ("Acceptance criteria", "acceptance"),
    ("Constraints / do not do", "constraint"),
    ("Dependencies", "dependenc"),
];

pub fn missing_recommended_sections(body: Option<&str>) -> Vec<String> {
    let headings = headings(body.unwrap_or(""));
    RECOMMENDED
        .iter()
        .filter(|(_, needle)| !headings.iter().any(|heading| heading.contains(needle)))
        .map(|(label, _)| (*label).to_string())
        .collect()
}

pub fn readiness_warnings(body: Option<&str>, risk_note: Option<String>) -> Vec<String> {
    let mut warnings = missing_recommended_sections(body)
        .into_iter()
        .map(|section| format!("missing recommended section: {section}"))
        .collect::<Vec<_>>();
    if let Some(note) = risk_note {
        warnings.push(note);
    }
    warnings
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

fn headings(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| heading_text(line.trim()))
        .collect()
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
    fn full_body_has_no_missing_sections_and_parses_criteria() {
        assert!(missing_recommended_sections(Some(FULL)).is_empty());
        assert_eq!(
            acceptance_criteria(Some(FULL)),
            vec![
                "Compare baseline, varint, and delta-coded timestamp streams".to_string(),
                "Do not change the production storage format".to_string(),
            ]
        );
    }

    #[test]
    fn sparse_body_lists_missing_sections() {
        let missing = missing_recommended_sections(Some("just an idea"));
        assert!(missing.iter().any(|item| item == "Goal"));
        assert!(missing.iter().any(|item| item == "Acceptance criteria"));
        assert!(missing_recommended_sections(None)
            .iter()
            .any(|item| item == "Goal"));
    }

    #[test]
    fn optional_sections_are_still_listed_when_other_headings_are_present() {
        let body = r#"
## Goal
G
## Scope
S
## Deliverable
D
## Acceptance criteria
- one
"#;
        let missing = missing_recommended_sections(Some(body));
        assert!(!missing.iter().any(|item| item == "Goal"));
        assert!(missing.iter().any(|item| item.contains("Constraints")));
        assert!(missing.iter().any(|item| item.contains("Dependencies")));
    }
}
