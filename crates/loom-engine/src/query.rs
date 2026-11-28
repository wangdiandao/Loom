//! Structured, backward-compatible process search query parser and evaluator.
//!
//! Queries are split on `,` into OR-combined clauses, and each clause is split
//! on whitespace (respecting double quotes) into AND-combined terms.
//!
//! Supported predicates:
//! - Plain case-insensitive substring across name, friendly display name, window
//!   title, executable path, publisher, command line, user name (when enabled),
//!   and numeric PID.
//! - Negation prefix `!term` (or `-term` when followed by a non-digit token).
//! - `pid:<u32>` exact process ID match.
//! - `user:<substr>` case-insensitive owner username substring match.
//! - `cpu:<op><f32>` (`>`, `>=`, `<`, `<=`, `=`) compared against CPU percentage.
//! - `mem:<op><size>` (`b`/`k`/`kb`/`m`/`mb`/`g`/`gb`, default MB) compared against
//!   private working set bytes.
//! - `status:<hung|suspended|eco|elevated|running|kernel>`.
//! - `:PORT` or `port:PORT` matching processes owning a listening socket on `PORT`.

use std::collections::HashSet;

use loom_types::ProcessStatusFlags;

/// Comparison operator used by numeric search predicates (`cpu:` / `mem:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// Strictly greater than (`>`).
    Gt,
    /// Greater than or equal (`>=`).
    Ge,
    /// Strictly less than (`<`).
    Lt,
    /// Less than or equal (`<=`).
    Le,
    /// Equal (`=`).
    Eq,
}

impl CompareOp {
    #[inline]
    fn eval_u64(self, lhs: u64, rhs: u64) -> bool {
        match self {
            Self::Gt => lhs > rhs,
            Self::Ge => lhs >= rhs,
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Eq => lhs == rhs,
        }
    }

    #[inline]
    fn eval_f32(self, lhs: f32, rhs: f32) -> bool {
        match self {
            Self::Gt => lhs > rhs,
            Self::Ge => lhs >= rhs,
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Eq => (lhs - rhs).abs() < 0.05,
        }
    }
}

/// Process status predicate recognized by `status:<keyword>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// Window is not responding (`NOT_RESPONDING`).
    Hung,
    /// All process threads are suspended (`SUSPENDED`).
    Suspended,
    /// Process is running in Efficiency Mode (`EFFICIENCY_MODE`).
    Eco,
    /// Process token is elevated (`ELEVATED`).
    Elevated,
    /// Process is actively running (neither suspended nor hung).
    Running,
    /// Core OS kernel process (`KERNEL`).
    Kernel,
}

impl StatusFilter {
    fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "hung" | "not_responding" | "notresponding" => Some(Self::Hung),
            "suspended" => Some(Self::Suspended),
            "eco" | "efficiency" => Some(Self::Eco),
            "elevated" | "admin" => Some(Self::Elevated),
            "running" => Some(Self::Running),
            "kernel" => Some(Self::Kernel),
            _ => None,
        }
    }

    #[inline]
    fn matches(self, status: ProcessStatusFlags) -> bool {
        match self {
            Self::Hung => status.contains(ProcessStatusFlags::NOT_RESPONDING),
            Self::Suspended => status.contains(ProcessStatusFlags::SUSPENDED),
            Self::Eco => status.contains(ProcessStatusFlags::EFFICIENCY_MODE),
            Self::Elevated => status.contains(ProcessStatusFlags::ELEVATED),
            Self::Running => !status
                .intersects(ProcessStatusFlags::SUSPENDED | ProcessStatusFlags::NOT_RESPONDING),
            Self::Kernel => status.contains(ProcessStatusFlags::KERNEL),
        }
    }
}

/// A single atomic match predicate inside a query clause.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryAtom {
    /// Plain case-insensitive substring across core process metadata and PID.
    Text(String),
    /// Exact numeric PID match (`pid:1234`).
    Pid(u32),
    /// Case-insensitive owner username substring (`user:admin`).
    User(String),
    /// CPU utilization percentage comparison (`cpu:>10`, `cpu:>=2.5`).
    Cpu {
        /// Comparison operator.
        op: CompareOp,
        /// Threshold in percent (`0.0..=100.0`).
        percent: f32,
    },
    /// Private working set memory comparison (`mem:>500m`, `mem:>1g`).
    Memory {
        /// Comparison operator.
        op: CompareOp,
        /// Threshold in bytes.
        bytes: u64,
    },
    /// Status flag filter (`status:hung`, `status:eco`, ...).
    Status(StatusFilter),
    /// On-demand listening TCP/UDP port match (`:3000` or `port:443`).
    Port(u16),
}

/// A single AND-term within a query clause, optionally negated.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryTerm {
    /// Whether this term's match result is inverted (`!term` / `-term`).
    pub negated: bool,
    /// Underlying atomic predicate.
    pub atom: QueryAtom,
}

/// One comma-delimited OR clause containing AND-combined terms.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryClause {
    /// All terms that must match simultaneously for this clause to match.
    pub terms: Vec<QueryTerm>,
}

/// Pre-parsed structured search query ready for fast per-process evaluation.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParsedQuery {
    clauses: Vec<QueryClause>,
    has_port: bool,
}

/// Borrowed per-process fields inspected during [`ParsedQuery::matches`].
#[derive(Debug, Clone, Copy)]
pub struct QueryMatchContext<'a> {
    /// Numeric process ID.
    pub pid: u32,
    /// Raw executable image name.
    pub name: &'a str,
    /// Friendly display name (file description / window title / name).
    pub display_name: &'a str,
    /// Optional primary top-level window title.
    pub window_title: Option<&'a str>,
    /// Optional PE publisher / company name.
    pub publisher: Option<&'a str>,
    /// Optional full executable path.
    pub exe_path: Option<&'a str>,
    /// Optional process command line.
    pub command_line: Option<&'a str>,
    /// Optional owning user account name.
    pub user_name: Option<&'a str>,
    /// Whether plain-text fallback terms also match `user_name`.
    pub include_user_in_text: bool,
    /// Effective dynamic process status flags.
    pub status: ProcessStatusFlags,
    /// CPU utilization in permille (`0..=1000`).
    pub cpu_permille: u16,
    /// Private working set memory in bytes.
    pub memory_bytes: u64,
    /// Optional lookup set of active `(pid, local_port)` listening sockets.
    pub listening_ports: Option<&'a HashSet<(u32, u16)>>,
}

impl ParsedQuery {
    /// Parses a raw search string into structured OR/AND predicates.
    pub fn parse(raw: &str) -> Self {
        let mut clauses = Vec::new();
        let mut has_port = false;

        for raw_clause in raw.split(',') {
            let tokens = tokenize_clause(raw_clause);
            if tokens.is_empty() {
                continue;
            }
            let mut terms = Vec::with_capacity(tokens.len());
            for (token, was_quoted) in tokens {
                if token.is_empty() {
                    continue;
                }
                let term = if was_quoted {
                    QueryTerm {
                        negated: false,
                        atom: QueryAtom::Text(token.to_lowercase()),
                    }
                } else {
                    parse_term(&token)
                };
                if matches!(term.atom, QueryAtom::Port(_)) {
                    has_port = true;
                }
                terms.push(term);
            }
            if !terms.is_empty() {
                clauses.push(QueryClause { terms });
            }
        }

        Self { clauses, has_port }
    }

    /// Returns `true` when the query contains no active clauses (matches all rows).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    /// Returns `true` when any clause references a `:PORT` or `port:PORT` predicate.
    ///
    /// Used by the coordinator to gate on-demand listening port enumeration strictly
    /// when a port search is active.
    #[inline]
    pub const fn has_port_predicate(&self) -> bool {
        self.has_port
    }

    /// Returns the parsed OR-separated clauses.
    #[inline]
    pub fn clauses(&self) -> &[QueryClause] {
        &self.clauses
    }

    /// Evaluates whether the process described by `ctx` matches this query.
    pub fn matches(&self, ctx: &QueryMatchContext<'_>) -> bool {
        if self.clauses.is_empty() {
            return true;
        }
        self.clauses.iter().any(|clause| {
            clause.terms.iter().all(|term| {
                let matched = eval_atom(&term.atom, ctx);
                if term.negated { !matched } else { matched }
            })
        })
    }
}

/// Splits a single comma-delimited clause into `(token, was_quoted)` items.
fn tokenize_clause(clause: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut chars = clause.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch == '"' {
            chars.next();
            let mut buf = String::new();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                buf.push(c);
            }
            if !buf.is_empty() {
                out.push((buf, true));
            }
        } else {
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                buf.push(c);
                chars.next();
            }
            if !buf.is_empty() {
                out.push((buf, false));
            }
        }
    }

    out
}

fn parse_term(raw: &str) -> QueryTerm {
    let (negated, body) = if let Some(rest) = raw.strip_prefix('!')
        && !rest.is_empty()
    {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix('-')
        && let Some(first) = rest.chars().next()
        && !first.is_ascii_digit()
    {
        (true, rest)
    } else {
        (false, raw)
    };

    let atom = parse_atom(body).unwrap_or_else(|| QueryAtom::Text(body.to_lowercase()));
    QueryTerm { negated, atom }
}

fn parse_atom(body: &str) -> Option<QueryAtom> {
    // Shorthand `:PORT` (e.g., `:3000`, `:443`).
    if let Some(port_str) = body.strip_prefix(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            return Some(QueryAtom::Port(port));
        }
        return None;
    }

    let (field, value) = body.split_once(':')?;
    if value.is_empty() {
        return None;
    }

    match field.to_ascii_lowercase().as_str() {
        "port" => value.parse::<u16>().ok().map(QueryAtom::Port),
        "pid" => value.parse::<u32>().ok().map(QueryAtom::Pid),
        "user" => Some(QueryAtom::User(value.to_lowercase())),
        "cpu" => parse_cpu_predicate(value),
        "mem" | "memory" => parse_mem_predicate(value),
        "status" => StatusFilter::parse(value).map(QueryAtom::Status),
        _ => None,
    }
}

fn parse_op_and_rest(raw: &str) -> Option<(CompareOp, &str)> {
    if let Some(rest) = raw.strip_prefix(">=") {
        Some((CompareOp::Ge, rest))
    } else if let Some(rest) = raw.strip_prefix("<=") {
        Some((CompareOp::Le, rest))
    } else if let Some(rest) = raw.strip_prefix('>') {
        Some((CompareOp::Gt, rest))
    } else if let Some(rest) = raw.strip_prefix('<') {
        Some((CompareOp::Lt, rest))
    } else if let Some(rest) = raw.strip_prefix('=') {
        Some((CompareOp::Eq, rest))
    } else {
        None
    }
}

fn parse_cpu_predicate(raw: &str) -> Option<QueryAtom> {
    let (op, rest) = parse_op_and_rest(raw)?;
    let num_str = rest.strip_suffix('%').unwrap_or(rest);
    let percent = num_str.parse::<f32>().ok()?;
    if !percent.is_finite() || percent < 0.0 {
        return None;
    }
    Some(QueryAtom::Cpu { op, percent })
}

fn parse_mem_predicate(raw: &str) -> Option<QueryAtom> {
    let (op, rest) = parse_op_and_rest(raw)?;
    if rest.is_empty() {
        return None;
    }

    let split_idx = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());
    let (num_part, unit_part) = rest.split_at(split_idx);
    let value = num_part.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let multiplier: f64 = match unit_part.to_ascii_lowercase().as_str() {
        "" | "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "k" | "kb" | "kib" => 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "b" => 1.0,
        _ => return None,
    };

    let scaled = (value * multiplier).round();
    if !scaled.is_finite() || scaled < 0.0 || scaled > u64::MAX as f64 {
        return None;
    }
    Some(QueryAtom::Memory {
        op,
        bytes: scaled as u64,
    })
}

fn eval_atom(atom: &QueryAtom, ctx: &QueryMatchContext<'_>) -> bool {
    match atom {
        QueryAtom::Text(needle_lower) => eval_text(needle_lower, ctx),
        QueryAtom::Pid(target) => ctx.pid == *target,
        QueryAtom::User(needle_lower) => ctx
            .user_name
            .is_some_and(|u| contains_case_insensitive(u, needle_lower)),
        QueryAtom::Cpu { op, percent } => {
            let cpu_percent = f32::from(ctx.cpu_permille) / 10.0;
            op.eval_f32(cpu_percent, *percent)
        }
        QueryAtom::Memory { op, bytes } => op.eval_u64(ctx.memory_bytes, *bytes),
        QueryAtom::Status(filter) => filter.matches(ctx.status),
        QueryAtom::Port(port) => ctx
            .listening_ports
            .is_some_and(|ports| ports.contains(&(ctx.pid, *port))),
    }
}

fn eval_text(needle_lower: &str, ctx: &QueryMatchContext<'_>) -> bool {
    if pid_matches_query(ctx.pid, needle_lower) {
        return true;
    }
    if contains_case_insensitive(ctx.name, needle_lower) {
        return true;
    }
    if !ctx.display_name.is_empty()
        && ctx.display_name != ctx.name
        && contains_case_insensitive(ctx.display_name, needle_lower)
    {
        return true;
    }
    if let Some(title) = ctx.window_title
        && contains_case_insensitive(title, needle_lower)
    {
        return true;
    }
    if let Some(publisher) = ctx.publisher
        && contains_case_insensitive(publisher, needle_lower)
    {
        return true;
    }
    if let Some(exe_path) = ctx.exe_path
        && contains_case_insensitive(exe_path, needle_lower)
    {
        return true;
    }
    if let Some(cmd) = ctx.command_line
        && contains_case_insensitive(cmd, needle_lower)
    {
        return true;
    }
    if ctx.include_user_in_text
        && let Some(user) = ctx.user_name
        && contains_case_insensitive(user, needle_lower)
    {
        return true;
    }
    false
}

/// Checks whether a numeric PID decimal representation contains `query_lower`
/// without heap allocation.
pub(crate) fn pid_matches_query(mut pid: u32, query_lower: &str) -> bool {
    if query_lower.is_empty() || !query_lower.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let mut buf = [0u8; 10];
    let mut idx = buf.len();
    if pid == 0 {
        idx -= 1;
        buf[idx] = b'0';
    } else {
        while pid > 0 {
            idx -= 1;
            buf[idx] = b'0' + (pid % 10) as u8;
            pid /= 10;
        }
    }
    let pid_str = std::str::from_utf8(&buf[idx..]).unwrap_or("");
    pid_str.contains(query_lower)
}

/// Case-insensitive substring test with a zero-allocation ASCII fast path.
pub(crate) fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let h = haystack.as_bytes();
        let n = needle_lower.as_bytes();
        if n.len() > h.len() {
            return false;
        }
        return h.windows(n.len()).any(|window| {
            window
                .iter()
                .zip(n.iter())
                .all(|(&hb, &nb)| hb.to_ascii_lowercase() == nb)
        });
    }
    haystack.to_lowercase().contains(needle_lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_ctx<'a>(ports: Option<&'a HashSet<(u32, u16)>>) -> QueryMatchContext<'a> {
        QueryMatchContext {
            pid: 4200,
            name: "node.exe",
            display_name: "Node.js JavaScript Runtime",
            window_title: None,
            publisher: Some("OpenJS Foundation"),
            exe_path: Some(r"C:\Program Files\nodejs\node.exe"),
            command_line: Some("node.exe server.js --port 3000"),
            user_name: Some(r"DESKTOP\Alice"),
            include_user_in_text: false,
            status: ProcessStatusFlags::EFFICIENCY_MODE | ProcessStatusFlags::ELEVATED,
            cpu_permille: 125,               // 12.5%
            memory_bytes: 600 * 1024 * 1024, // 600 MiB
            listening_ports: ports,
        }
    }

    #[test]
    fn empty_and_plain_substring_queries_remain_backward_compatible() {
        let ctx = sample_ctx(None);
        assert!(ParsedQuery::parse("").is_empty());
        assert!(ParsedQuery::parse("   ,  ").is_empty());
        assert!(ParsedQuery::parse("").matches(&ctx));
        assert!(ParsedQuery::parse("NODE").matches(&ctx));
        assert!(ParsedQuery::parse("openjs").matches(&ctx));
        assert!(ParsedQuery::parse("server.js").matches(&ctx));
        assert!(ParsedQuery::parse("420").matches(&ctx));
        assert!(!ParsedQuery::parse("chrome").matches(&ctx));
    }

    #[test]
    fn or_and_negation_combinators_work_accurately() {
        let ctx = sample_ctx(None);

        // Comma separates OR clauses; spaces separate AND terms.
        let q = ParsedQuery::parse("chrome, node !svchost");
        assert!(!q.has_port_predicate());
        assert!(q.matches(&ctx));

        let negated_match = ParsedQuery::parse("node !openjs");
        assert!(!negated_match.matches(&ctx));

        let dash_negation = ParsedQuery::parse("node -openjs");
        assert!(!dash_negation.matches(&ctx));

        // `-3000` starts with a digit after `-` so it is not treated as negation.
        let dash_digit = ParsedQuery::parse("-3000");
        assert!(!dash_digit.clauses()[0].terms[0].negated);
    }

    #[test]
    fn structured_field_predicates_parse_and_evaluate() {
        let ctx = sample_ctx(None);

        assert!(ParsedQuery::parse("pid:4200").matches(&ctx));
        assert!(!ParsedQuery::parse("pid:420").matches(&ctx));

        assert!(ParsedQuery::parse("user:alice").matches(&ctx));
        assert!(!ParsedQuery::parse("user:system").matches(&ctx));

        assert!(ParsedQuery::parse("cpu:>10").matches(&ctx));
        assert!(ParsedQuery::parse("cpu:>=12.5").matches(&ctx));
        assert!(ParsedQuery::parse("cpu:=12.5%").matches(&ctx));
        assert!(!ParsedQuery::parse("cpu:<10").matches(&ctx));

        assert!(ParsedQuery::parse("mem:>500m").matches(&ctx));
        assert!(ParsedQuery::parse("mem:>=600").matches(&ctx));
        assert!(ParsedQuery::parse("mem:<1g").matches(&ctx));
        assert!(!ParsedQuery::parse("mem:>1gb").matches(&ctx));
        assert!(ParsedQuery::parse("mem:>1000k").matches(&ctx));

        assert!(ParsedQuery::parse("status:eco status:elevated status:running").matches(&ctx));
        assert!(!ParsedQuery::parse("status:hung").matches(&ctx));
        assert!(!ParsedQuery::parse("status:suspended").matches(&ctx));
        assert!(!ParsedQuery::parse("status:kernel").matches(&ctx));
    }

    #[test]
    fn port_predicates_set_flag_and_match_socket_table() {
        let mut ports = HashSet::new();
        ports.insert((4200, 3000));
        ports.insert((4200, 443));

        let q1 = ParsedQuery::parse(":3000");
        assert!(q1.has_port_predicate());
        assert!(q1.matches(&sample_ctx(Some(&ports))));
        assert!(!q1.matches(&sample_ctx(None)));

        let q2 = ParsedQuery::parse("port:443 node");
        assert!(q2.has_port_predicate());
        assert!(q2.matches(&sample_ctx(Some(&ports))));

        let q3 = ParsedQuery::parse(":8080, :3000");
        assert!(q3.has_port_predicate());
        assert!(q3.matches(&sample_ctx(Some(&ports))));

        let q4 = ParsedQuery::parse(":9999");
        assert!(q4.has_port_predicate());
        assert!(!q4.matches(&sample_ctx(Some(&ports))));
    }
}
