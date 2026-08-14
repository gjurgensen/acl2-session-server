//! Sync a real book and print the structured report for every unit.
//!
//!   acl2-session-syncfile <acl2-binary> <book.lisp> [--output <ref>] [--find <text>]

use std::path::PathBuf;
use std::time::Duration;

use acl2_session::{Book, Slice};

fn wrap(label: &str, s: &str, indent: usize, width: usize) {
    let pad = " ".repeat(indent);
    let mut line = String::new();
    let mut first = true;
    for tok in s.split_whitespace() {
        if line.len() + tok.len() + 1 > width && !line.is_empty() {
            println!("{pad}{:<14}{line}", if first { label } else { "" });
            first = false;
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(tok);
    }
    if !line.is_empty() {
        println!("{pad}{:<14}{line}", if first { label } else { "" });
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: acl2-session-syncfile <acl2-binary> <book.lisp> [--find <text>]");
        std::process::exit(2);
    }
    let acl2 = PathBuf::from(&a[1]);
    let book = PathBuf::from(&a[2]);
    let find = a.iter().position(|x| x == "--find").and_then(|i| a.get(i + 1)).cloned();

    let mut b = Book::open(&acl2, &book).expect("open book");
    let o = b.sync(None, Duration::from_secs(300)).expect("sync");

    println!("\n{}  ({} units synced)\n", book.display(),
             o.admitted.len() + usize::from(o.failed.is_some()));

    for u in &o.admitted {
        println!("unit {}  lines {}-{}  OK  {} ms  [{} | {} lines]",
                 u.index, u.line_start, u.line_end, u.elapsed.as_millis(),
                 u.output_ref, u.output_lines);
        let e = &u.event_data;
        if let Some(s) = e.prover_steps { println!("    {:<14}{s}", "steps"); }
        if let Some(t) = &e.time { wrap("time", t, 4, 70); }
        match &e.rules {
            Some(r) => wrap("rules", r, 4, 70),
            None => println!("    {:<14}(not reported)", "rules"),
        }
        if let Some(w) = &e.warnings { wrap("warnings", w, 4, 70); }
        for ev in &u.inner {
            let r = ev.rules.as_deref().unwrap_or("nil");
            wrap(&format!("  {}", ev.name), r, 4, 70);
        }
        if u.inner_total > u.inner.len() {
            println!("    {:<14}({} shown of {}+)", "", u.inner.len(), u.inner_total);
        }
        println!();
    }

    if let Some(f) = &o.failed {
        println!("unit {}  lines {}-{}  {:?}  [{} | {} lines]",
                 f.index, f.line_start, f.line_end, f.status,
                 f.output_ref, f.output_lines);
        if let Some(e) = &f.error_line { wrap("error", e, 4, 70); }
        let c = &f.checkpoints;
        if let Some(x) = &c.abort_cause { println!("    {:<14}{x}", "abort-cause"); }
        if let Some(s) = f.event_data.prover_steps { println!("    {:<14}{s}", "steps"); }
        for (i, g) in c.top.iter().enumerate() {
            wrap(&format!("top[{i}]/{}", c.n_top), g, 4, 70);
        }
        for (i, g) in c.sub.iter().enumerate() {
            wrap(&format!("sub[{i}]/{}", c.n_sub), g, 4, 70);
        }
        if c.forced { println!("    {:<14}yes", "forced"); }
        if let Some(ev) = f.failed_event() {
            println!("    {:<14}{}", "failed event", ev.name);
        }
        for ev in &f.inner {
            let mark = if ev.abort_causes.is_some() { "!" } else { " " };
            println!("    {:<14}{mark} {} {}", "  inner", ev.name,
                     ev.abort_causes.as_deref().unwrap_or(""));
        }
        println!();
    }

    if let Some(needle) = find {
        let refs: Vec<_> = o.admitted.iter().map(|u| (u.index, &u.output_ref))
            .chain(o.failed.iter().map(|f| (f.index, &f.output_ref)))
            .collect();
        for (i, r) in refs {
            let sl = b.output(r, Slice::Find {
                needle: needle.clone(), context: 1, ignore_case: true,
            }).expect("slice");
            if !sl.text.trim().is_empty() {
                println!("--- unit {i} output, {:?} (of {} lines) ---", needle, sl.total_lines);
                for l in sl.text.lines() { println!("  | {}", l.trim_end()); }
            }
        }
    }
}
