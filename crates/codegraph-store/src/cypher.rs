//! Cypher-lite: a read-only openCypher SUBSET translated to SQL over nodes/edges.
//! Covers the agent-useful 80%: 1–2 hop MATCH patterns with labels/relations,
//! WHERE on name/file (=, CONTAINS, STARTS WITH), RETURN props, LIMIT.
//! Unsupported syntax → clear error (never a wrong answer).

/// `MATCH (a:Label)-[:REL]->(b:Label)-[:REL2]->(c) WHERE a.name = 'x' RETURN a.name, b.file LIMIT 10`
pub fn to_sql(q: &str) -> Result<String, String> {
    let q = q.trim();
    let lower = q.to_lowercase();
    if !lower.starts_with("match") {
        return Err("only MATCH … [WHERE …] RETURN … [LIMIT n] is supported".into());
    }
    let ret_pos = lower.find(" return ").ok_or("missing RETURN")?;
    let (head, tail) = (&q[5..ret_pos], &q[ret_pos + 8..]);
    let (where_part, pattern) = match head.to_lowercase().find(" where ") {
        Some(w) => (Some(head[w + 7..].trim()), head[..w].trim()),
        None => (None, head.trim()),
    };
    let (ret_part, limit) = match tail.to_lowercase().find(" limit ") {
        Some(l) => (
            tail[..l].trim(),
            tail[l + 7..].trim().parse::<u32>().map_err(|_| "bad LIMIT")?,
        ),
        None => (tail.trim(), 50),
    };

    // ---- pattern: (var[:Label]) ( -[:REL]-> (var[:Label]) )* , max 2 hops ----
    let mut vars: Vec<(String, Option<String>)> = Vec::new(); // (var, label)
    let mut rels: Vec<Option<String>> = Vec::new();
    let mut rest = pattern.trim();
    loop {
        if !rest.starts_with('(') {
            return Err(format!("expected '(' at: {rest}"));
        }
        let close = rest.find(')').ok_or("unclosed node pattern")?;
        let inner = &rest[1..close];
        let (var, label) = match inner.split_once(':') {
            Some((v, l)) => (v.trim().to_string(), Some(l.trim().to_string())),
            None => (inner.trim().to_string(), None),
        };
        if var.is_empty() {
            return Err("node variables are required, e.g. (a:Function)".into());
        }
        vars.push((var, label));
        rest = rest[close + 1..].trim();
        if rest.is_empty() {
            break;
        }
        // -[:REL]-> or -->
        if let Some(r) = rest.strip_prefix("-[") {
            let close = r.find("]->").ok_or("only outgoing -[:REL]-> supported")?;
            let rel = r[..close].trim().trim_start_matches(':').trim().to_string();
            rels.push((!rel.is_empty()).then_some(rel));
            rest = r[close + 3..].trim();
        } else if let Some(r) = rest.strip_prefix("-->") {
            rels.push(None);
            rest = r.trim();
        } else {
            return Err(format!("expected -[:REL]-> at: {rest}"));
        }
        if vars.len() > 3 {
            return Err("max 2 hops (3 nodes) in cypher-lite".into());
        }
    }

    // ---- SQL assembly ----
    let mut from = vec![format!("nodes {}", vars[0].0)];
    let mut conds: Vec<String> = Vec::new();
    for (i, rel) in rels.iter().enumerate() {
        let e = format!("e{i}");
        let (a, b) = (&vars[i].0, &vars[i + 1].0);
        from.push(format!("edges {e}"));
        from.push(format!("nodes {b}"));
        conds.push(format!("{e}.src = {a}.id AND {e}.dst = {b}.id"));
        if let Some(r) = rel {
            conds.push(format!("{e}.relation = '{}'", sane(r)?));
        }
    }
    for (v, label) in &vars {
        if let Some(l) = label {
            conds.push(format!("{v}.label = '{}'", sane(l)?));
        }
    }
    if let Some(w) = where_part {
        conds.push(where_to_sql(w)?);
    }

    let cols: Vec<String> = ret_part
        .split(',')
        .map(|c| prop_to_sql(c.trim()))
        .collect::<Result<_, _>>()?;
    let where_sql = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
    Ok(format!(
        "SELECT DISTINCT {} FROM {}{} LIMIT {}",
        cols.join(", "),
        from.join(", "),
        where_sql,
        limit.min(500)
    ))
}

/// Split a WHERE body on ` AND ` case-insensitively, skipping occurrences
/// inside single-quoted string values (`name = 'a and b'`).
fn split_and(w: &str) -> Vec<&str> {
    let lower = w.to_lowercase();
    let mut parts = Vec::new();
    let (mut start, mut from) = (0usize, 0usize);
    while let Some(p) = lower[from..].find(" and ") {
        let pos = from + p;
        if w[..pos].matches('\'').count() % 2 == 1 {
            from = pos + 5; // inside a quoted value — not a conjunction
            continue;
        }
        parts.push(&w[start..pos]);
        start = pos + 5;
        from = start;
    }
    parts.push(&w[start..]);
    parts
}

/// `a.name = 'x' AND b.file CONTAINS 'auth'` → SQL conjunction (AND-only).
fn where_to_sql(w: &str) -> Result<String, String> {
    let mut parts = Vec::new();
    for clause in split_and(w) {
        let c = clause.trim();
        let lower = c.to_lowercase();
        let (op_pos, sql_op, op_len, like) = if let Some(p) = lower.find(" contains ") {
            (p, "LIKE", 10, Some(("%", "%")))
        } else if let Some(p) = lower.find(" starts with ") {
            (p, "LIKE", 13, Some(("", "%")))
        } else if let Some(p) = c.find('=') {
            (p, "=", 1, None)
        } else {
            return Err(format!("unsupported WHERE clause: {c} (use =, CONTAINS, STARTS WITH, AND)"));
        };
        let lhs = prop_to_sql(c[..op_pos].trim())?;
        let raw = c[op_pos + op_len..].trim().trim_matches('\'').trim_matches('"');
        match like {
            // LIKE: escape the user's % _ \ so CONTAINS '100%' matches literally.
            Some((pre, post)) => {
                let val = raw
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
                    .replace('\'', "''");
                parts.push(format!("{lhs} {sql_op} '{pre}{val}{post}' ESCAPE '\\'"));
            }
            None => {
                let val = raw.replace('\'', "''");
                parts.push(format!("{lhs} {sql_op} '{val}'"));
            }
        }
    }
    Ok(format!("({})", parts.join(" AND ")))
}

/// `a.name` → `a.name`, mapping cypher prop names onto real columns.
fn prop_to_sql(p: &str) -> Result<String, String> {
    let (var, prop) = p.split_once('.').ok_or_else(|| format!("use var.prop, got: {p}"))?;
    let var = var.trim();
    if !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("bad variable: {var}"));
    }
    let col = match prop.trim() {
        "name" => "name",
        "file" | "file_path" => "file_path",
        "line" | "line_start" => "line_start",
        "label" | "kind" => "label",
        "language" | "lang" => "language",
        "id" => "id",
        "pagerank" => "pagerank",
        other => return Err(format!("unknown property: {other} (name/file/line/label/language/id/pagerank)")),
    };
    Ok(format!("{var}.{col}"))
}

fn sane(s: &str) -> Result<&str, String> {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(s)
    } else {
        Err(format!("bad identifier: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::to_sql;

    #[test]
    fn one_hop_with_where() {
        let sql = to_sql("MATCH (a:Function)-[:Calls]->(b) WHERE b.name = 'save' RETURN a.name, a.file LIMIT 5").unwrap();
        assert!(sql.contains("e0.relation = 'Calls'"));
        assert!(sql.contains("b.name = 'save'"));
        assert!(sql.contains("LIMIT 5"));
    }

    #[test]
    fn two_hop_and_contains() {
        let sql = to_sql("MATCH (a)-[:Calls]->(b)-[:Calls]->(c) WHERE a.file CONTAINS 'auth' RETURN c.name").unwrap();
        assert!(sql.contains("e1.src = b.id"));
        assert!(sql.contains("LIKE '%auth%'"));
    }

    #[test]
    fn injection_rejected() {
        assert!(to_sql("MATCH (a:Fn'; DROP TABLE nodes;--)-->(b) RETURN a.name").is_err());
    }

    #[test]
    fn lowercase_and_splits() {
        let sql = to_sql("MATCH (a)-->(b) WHERE a.name = 'x' and b.name = 'y' RETURN a.name").unwrap();
        assert!(sql.contains("a.name = 'x'") && sql.contains("b.name = 'y'"), "{sql}");
    }

    #[test]
    fn and_inside_quoted_value_not_split() {
        let sql = to_sql("MATCH (a)-->(b) WHERE a.name = 'salt and pepper' RETURN a.name").unwrap();
        assert!(sql.contains("'salt and pepper'"), "{sql}");
    }

    #[test]
    fn like_wildcards_escaped() {
        let sql = to_sql("MATCH (a)-->(b) WHERE a.file CONTAINS '100%_x' RETURN a.name").unwrap();
        assert!(sql.contains("LIKE '%100\\%\\_x%' ESCAPE '\\'"), "{sql}");
    }
}
