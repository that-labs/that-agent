//! Plan management — lightweight plan file scanner for autonomous agents.
//!
//! Agents create `plan-{n}.md` files in their agent directory as working memory.
//! This module scans those files and extracts summaries for preamble injection.

/// Summary of a single plan file.
pub struct PlanSummary {
    pub number: u16,
    pub title: String,
    pub status: String,
    pub steps_total: usize,
    pub steps_done: usize,
    pub variables: Vec<(String, String)>,
}

/// Scan `plan-*.md` in the local agent directory and return active plan summaries.
pub fn scan_plans_local(agent_name: &str) -> Vec<PlanSummary> {
    let Some(dir) = dirs::home_dir().map(|h| h.join(".that-agent").join("agents").join(agent_name))
    else {
        return vec![];
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return vec![];
    };

    let mut plans: Vec<PlanSummary> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_str().unwrap_or_default();
        let num = match name
            .strip_prefix("plan-")
            .and_then(|s| s.strip_suffix(".md"))
            .and_then(|s| s.parse::<u16>().ok())
        {
            Some(n) => n,
            None => continue,
        };

        let Ok(content) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let summary = parse_plan(&content, num);
        if summary.status == "active" {
            plans.push(summary);
        }
    }
    plans.sort_by_key(|p| p.number);
    plans
}

fn parse_plan(content: &str, number: u16) -> PlanSummary {
    let mut title = String::new();
    let mut status = String::from("active");
    let mut steps_total = 0usize;
    let mut steps_done = 0usize;
    let mut variables = Vec::new();
    let mut in_variables = false;

    for line in content.lines() {
        if line.starts_with("# ") && title.is_empty() {
            title = line[2..].trim().to_string();
        } else if let Some(rest) = line.strip_prefix("**Status**:") {
            status = rest.trim().to_lowercase();
        } else if line.starts_with("## Variables") {
            in_variables = true;
        } else if line.starts_with("## ") {
            in_variables = false;
        } else if line.starts_with("- [x]") || line.starts_with("- [X]") {
            steps_total += 1;
            steps_done += 1;
        } else if line.starts_with("- [ ]") {
            steps_total += 1;
        } else if in_variables {
            if let Some((k, v)) = line.strip_prefix("- ").and_then(|s| s.split_once(": ")) {
                variables.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }

    PlanSummary {
        number,
        title,
        status,
        steps_total,
        steps_done,
        variables,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_extracts_fields() {
        let content = "# Deploy monitoring stack\n\
                        **Status**: active\n\n\
                        - [x] Set up Prometheus\n\
                        - [x] Configure Grafana\n\
                        - [ ] Add alerting rules\n\
                        - [ ] Verify dashboards\n\n\
                        ## Variables\n\
                        - namespace: monitoring\n\
                        - version: 2.45.0\n";
        let s = parse_plan(content, 1);
        assert_eq!(s.title, "Deploy monitoring stack");
        assert_eq!(s.status, "active");
        assert_eq!(s.steps_total, 4);
        assert_eq!(s.steps_done, 2);
        assert_eq!(s.variables.len(), 2);
        assert_eq!(s.variables[0], ("namespace".into(), "monitoring".into()));
    }

    #[test]
    fn parse_plan_done_status_excluded() {
        let content = "# Old plan\n**Status**: done\n- [x] Step\n";
        let s = parse_plan(content, 2);
        assert_eq!(s.status, "done");
    }
}
