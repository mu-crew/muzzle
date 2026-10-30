//! The text rendering of a [`Report`]; `--json` serialises it as is.

use super::Report;

pub fn render_text(r: &Report) -> String {
    let opt = |v: Option<u64>| v.map_or("?".to_string(), |v| v.to_string());
    let mut out = String::new();
    let mut line = |k: &str, v: String| out.push_str(&format!("{k:<14} {v}\n"));
    line(
        "budget",
        format!(
            "{} slots ({}; {} cores, reserve {}, {} MB RAM per slot)",
            opt(r.budget.map(u64::from)),
            r.budget_source,
            opt(r.cores.map(u64::from)),
            opt(r.reserve.map(u64::from)),
            opt(r.ram_per_slot_mb),
        ),
    );
    line("enforcement", r.enforcement.clone());
    line(
        "state",
        format!(
            "{} ({})",
            r.state_dir.display(),
            match (r.state_writable, r.state_readable) {
                (false, _) => "NOT writable",
                (true, false) => "writable, state.json unreadable",
                (true, true) => "writable",
            }
        ),
    );
    let config = match (&r.config_error, r.config_exists) {
        (Some(_), _) => "invalid".to_string(),
        (None, exists) => format!(
            "{}; {} rules, {} path tables, hash {}",
            if exists {
                "found"
            } else {
                "not found: defaults"
            },
            opt(r.rules.map(|n| n as u64)),
            opt(r.path_tables.map(|n| n as u64)),
            r.config_hash.as_deref().unwrap_or("?"),
        ),
    };
    line("config", format!("{} ({config})", r.config_path.display()));
    for c in r.config_files.iter().filter(|c| c.role == "include") {
        line(
            "include",
            format!(
                "{} ({})",
                c.file.display(),
                if c.found { "found" } else { "not found" }
            ),
        );
    }
    line(
        "runs.jsonl",
        match &r.runs {
            None => "none yet".into(),
            Some(l) => format!(
                "{} bytes, {} runs, {} .. {}",
                l.bytes,
                l.lines,
                l.oldest.as_deref().unwrap_or("?"),
                l.newest.as_deref().unwrap_or("?")
            ),
        },
    );
    if let Some(l) = r.runs_archives.as_ref().filter(|l| l.archives > 0) {
        line(
            "archives",
            format!(
                "{} files, {} bytes, oldest {}",
                l.archives,
                l.archive_bytes,
                l.oldest_month.as_deref().unwrap_or("?")
            ),
        );
    }
    let mut containers = Vec::new();
    if !r.container_rules.is_empty() {
        containers.push(format!(
            "admission only for {} (D16a): the job runs in the container's cgroup, \
             so MemoryMax, CPUWeight and the cpu/mem figures don't reach it",
            r.container_rules.join(", ")
        ));
    }
    if !r.container_maybe.is_empty() {
        containers.push(format!(
            "{} may enter containers; those runs are admission only (D16a)",
            r.container_maybe.join(", ")
        ));
    }
    if !containers.is_empty() {
        line("containers", containers.join("; "));
    }
    line("pi extension", r.pi_extension.into());
    line("claude hook", r.claude_hook.into());
    line("codex hook", r.codex_hook.into());
    line("platform", r.platform.clone());
    for w in &r.warnings {
        out.push_str(&format!("warning: {w}\n"));
    }
    out
}
