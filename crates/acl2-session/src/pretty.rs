//! Lay an ACL2 term out at a given width, the way ACL2 would have.
//!
//! ACL2 prints for a 77-column terminal. An editor pane is rarely that, so a
//! term that fitted on one line for ACL2 arrives as one long line and gets
//! soft-wrapped by the client at whatever column the pane happens to be --
//! mid-argument, with no indentation to show what belongs to what. This lays
//! it out again at the width it is actually being read at.
//!
//! # Whose algorithm this is
//!
//! ACL2's. The shape here follows `ppr1`/`ppr2` in the ACL2 sources
//! (`basis-a.lisp`, and the Essay on the ACL2 Prettyprinter above them),
//! by way of `books/std/strings/pretty.lisp`, which is the same algorithm
//! restated without channels, `state` or evisceration. The algorithm is
//! Boyer's, from about 1971: two linear passes, the first building a tuple
//! that tells the second how to print.
//!
//! Following ACL2 rather than inventing a layout is a deliberate choice, and
//! the reason is the same one that puts the panel's colors on the editor's own
//! grammar and theme: a term in the panel and the same term in ACL2's output
//! should agree by construction, not by resemblance. It also means the rules
//! are *ACL2's* to justify, and there is a reference implementation to test
//! against.
//!
//! Its principles, in ACL2's own words:
//!
//! - Print flat whenever possible.
//! - However, don't print flat argument lists of length over 40; they're too
//!   hard to parse.
//! - Parenthesized expressions should not be printed on a line with any other
//!   argument (unless the whole form fits on the line).
//!
//! The last two are not consequences of the width -- they fire with the page
//! half empty -- and they are what makes a big goal read as structure. Notably
//! there is no rule forcing any particular head to break: `(and (natp x) (natp
//! y))` prints flat when it fits, and a clause's hypotheses land one per line
//! only because the `and` around them is over the flat margin and each
//! hypothesis is parenthesized.
//!
//! # What is different here
//!
//! This works on **text**, not on a Lisp object. ACL2 has the object and must
//! work out how wide each piece would print; we have what ACL2 already
//! printed, so an atom's width is just its length. That removes print-base,
//! print-radix, escaping and evisceration entirely, and it lets every node
//! carry its exact flat width, computed once as it is parsed. ACL2 cannot do
//! that -- it re-measures at every node, which is why `flsz1` takes a maximum
//! and stops counting once it exceeds it.
//!
//! Deliberately not implemented, none of which changes what a term *says*:
//!
//! - `ppr-special-syms` (`defun . 2`, `if . 2`, `let . 1` ...), the table
//!   saying how many leading arguments stay on the head's line. Its absence
//!   makes those forms lay out like any other; adding it later is an extra
//!   tuple shape, not a change to this one.
//! - Keyword pairs. ACL2 keeps `:key value` together on a line; here a keyword
//!   is an atom like any other, so a keyword argument list breaks one item per
//!   line rather than one pair per line.
//! - Evisceration and iprinting, which belong to a printer that owns the
//!   object.
//!
//! # Total
//!
//! Anything this cannot read comes back exactly as it arrived: unbalanced
//! parens, an unterminated string, tokens left over after the first term, or
//! nesting past [`MAX_DEPTH`]. A term this cannot parse is a term ACL2 printed
//! and we would rather show verbatim than mangle.

/// The widest a run of items merged onto one line may be, whatever the page
/// width. ACL2's `ppr-flat-right-margin`, whose default is 40 and whose
/// rationale is that longer flat argument lists are too hard to parse.
pub const FLAT_RIGHT_MARGIN: usize = 40;

/// The narrowest page this will lay out for. Below this the output is mostly
/// indentation, and a caller reporting a width this small has measured
/// something other than a pane.
const MIN_WIDTH: usize = 20;

/// How deep a term may nest before this gives up and returns it unchanged.
///
/// Both passes recurse on depth, and a term is not worth a blown stack.
/// Comfortably past anything ACL2 prints for a form a person wrote; reached
/// only by generated terms, which are the ones nobody reads anyway.
const MAX_DEPTH: usize = 400;

/// Lay `term` out for a page `cols` wide, or return it unchanged.
pub fn pretty(term: &str, cols: usize) -> String {
    let Some(node) = parse(term) else {
        return term.to_string();
    };
    let width = cols.max(MIN_WIDTH);
    let mut out = String::with_capacity(term.len() + term.len() / 4);
    ppr2(&ppr1(&node, width, 0), 0, &mut out);
    out
}

// ---- The term ----------------------------------------------------------

/// A parsed term, carrying the width it would print at on one line.
///
/// The width is exact and computed bottom-up as the node is built, so nothing
/// downstream ever measures anything. That is the one place this is simpler
/// than ACL2, which has to walk a subterm to find out how wide it prints.
#[derive(Debug)]
struct Node {
    flat: usize,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Atom(String),
    /// A reader macro and what it applies to: `'x`, `` `x ``, `,x`, `,@x`,
    /// `#'x`. Held as structure rather than as two atoms because they print
    /// with no space between them, and treating the prefix as an ordinary
    /// atom inserts one -- which changes the term.
    Prefix(String, Box<Node>),
    /// `items`, and a `tail` when the list is dotted: `(a b . c)`.
    List(Vec<Node>, Option<Box<Node>>),
}

impl Node {
    fn atom(text: String) -> Node {
        Node { flat: text.chars().count(), kind: Kind::Atom(text) }
    }

    fn prefix(p: String, inner: Node) -> Node {
        Node { flat: p.chars().count() + inner.flat, kind: Kind::Prefix(p, Box::new(inner)) }
    }

    fn list(items: Vec<Node>, tail: Option<Node>) -> Node {
        // Two parens, the items, a space between each pair of them, and for a
        // dotted list the " . " before the tail.
        let mut flat = 2 + items.iter().map(|i| i.flat).sum::<usize>()
            + items.len().saturating_sub(1);
        if let Some(t) = &tail {
            flat += 3 + t.flat;
        }
        Node { flat, kind: Kind::List(items, tail.map(Box::new)) }
    }

    /// Whether this prints like an atom -- the test that decides whether it
    /// may share a line with something else. A quoted constant is a list to
    /// ACL2 and so is not atom-like, however short it looks.
    fn atom_like(&self) -> bool {
        matches!(self.kind, Kind::Atom(_))
    }

    /// Write the one-line form, exactly `self.flat` columns wide.
    fn write_flat(&self, out: &mut String) {
        match &self.kind {
            Kind::Atom(s) => out.push_str(s),
            Kind::Prefix(p, inner) => {
                out.push_str(p);
                inner.write_flat(out);
            }
            Kind::List(items, tail) => {
                out.push('(');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    item.write_flat(out);
                }
                if let Some(t) = tail {
                    out.push_str(" . ");
                    t.write_flat(out);
                }
                out.push(')');
            }
        }
    }
}

// ---- Reading -----------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Tok {
    Open,
    Close,
    Atom(String),
    /// A reader macro, which binds to whatever follows it.
    Prefix(String),
}

/// Split into parens, reader macros and atoms.
///
/// Strings, `|bar quoted|` symbols and `#\c` character literals each hide
/// characters that must not be read as structure -- the same set the splitter
/// and the console's balance check have to know about.
fn lex(src: &str) -> Option<Vec<Tok>> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '(' {
            out.push(Tok::Open);
            i += 1;
            continue;
        }
        if c == ')' {
            out.push(Tok::Close);
            i += 1;
            continue;
        }
        // Reader macros, longest first: `,@` must be seen before `,`, and the
        // character literal `#\x` before the function quote `#'`.
        if c != '#' || b.get(i + 1) != Some(&'\\') {
            let two: Option<&str> = match (c, b.get(i + 1)) {
                (',', Some('@')) => Some(",@"),
                ('#', Some('\'')) => Some("#'"),
                _ => None,
            };
            if let Some(p) = two {
                out.push(Tok::Prefix(p.to_string()));
                i += 2;
                continue;
            }
            if c == '\'' || c == '`' || c == ',' {
                out.push(Tok::Prefix(c.to_string()));
                i += 1;
                continue;
            }
        }
        // An atom runs to the next delimiter, with the quoting forms opaque.
        let start = i;
        while i < b.len() && !b[i].is_whitespace() && b[i] != '(' && b[i] != ')' {
            if b[i] == '"' || b[i] == '|' {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == '\\' { 2 } else { 1 };
                }
                if i >= b.len() {
                    return None; // unterminated
                }
                i += 1;
            } else if b[i] == '#' && b.get(i + 1) == Some(&'\\') {
                i += 3;
            } else {
                i += 1;
            }
        }
        if i <= start {
            return None;
        }
        out.push(Tok::Atom(b[start..i.min(b.len())].iter().collect()));
    }
    Some(out)
}

/// Read exactly one term, or give up.
///
/// Gives up on unbalanced parens, on anything left over after the first term,
/// and on nesting past `MAX_DEPTH` -- all of which mean "show it as ACL2
/// printed it".
fn parse(src: &str) -> Option<Node> {
    let toks = lex(src)?;
    let mut i = 0;
    let node = parse_one(&toks, &mut i, 0)?;
    if i == toks.len() {
        Some(node)
    } else {
        None
    }
}

fn parse_one(toks: &[Tok], i: &mut usize, depth: usize) -> Option<Node> {
    if depth > MAX_DEPTH {
        return None;
    }
    match toks.get(*i)? {
        Tok::Close => None,
        Tok::Atom(s) => {
            *i += 1;
            Some(Node::atom(s.clone()))
        }
        Tok::Prefix(p) => {
            let p = p.clone();
            *i += 1;
            Some(Node::prefix(p, parse_one(toks, i, depth + 1)?))
        }
        Tok::Open => {
            *i += 1;
            let mut items = Vec::new();
            let mut tail = None;
            loop {
                match toks.get(*i)? {
                    Tok::Close => {
                        *i += 1;
                        return Some(Node::list(items, tail));
                    }
                    // A lone `.` after at least one item, with exactly one
                    // term and the close paren behind it, is a dotted tail.
                    // Anything else spelled `.` is just an atom: this has to
                    // stay total on text nobody promised was a term.
                    Tok::Atom(s)
                        if s == "."
                            && !items.is_empty()
                            && tail.is_none()
                            && matches!(toks.get(*i + 2), Some(Tok::Close)) =>
                    {
                        *i += 1;
                        tail = Some(parse_one(toks, i, depth + 1)?);
                    }
                    _ => items.push(parse_one(toks, i, depth + 1)?),
                }
            }
        }
    }
}

// ---- Pass one: the tuple -----------------------------------------------

/// One thing printed on a flat line: an object, or the dot of a dotted list.
#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    Dot,
    Obj(&'a Node),
}

impl Item<'_> {
    /// May this share a line with what is already there? ACL2's rule, and the
    /// whole of its "parenthesized expressions should not be printed on a line
    /// with any other argument".
    fn atom_like(&self) -> bool {
        match self {
            Item::Dot => true,
            Item::Obj(n) => n.atom_like(),
        }
    }

    fn write_flat(&self, out: &mut String) {
        match self {
            Item::Dot => out.push('.'),
            Item::Obj(n) => n.write_flat(out),
        }
    }
}

/// How to print a term: ACL2's ppr tuple.
///
/// Every variant records the width of the longest line it will print,
/// measured from where it starts -- which is what lets the pass above decide
/// between shapes without printing anything.
#[derive(Debug)]
enum Tuple<'a> {
    /// One or more objects on a single line, separated by spaces.
    Flat { width: usize, items: Vec<Item<'a>> },
    /// `(head arg…)` with the arguments in a column aligned after the head.
    Wide { width: usize, head: Box<Tuple<'a>>, cols: Vec<Tuple<'a>> },
    /// `(head` then the arguments in a column `indent` to the right of the
    /// open paren. Used when the head is not a short atom, or when aligning
    /// after it would leave the arguments no room.
    Indent { width: usize, indent: usize, head: Box<Tuple<'a>>, cols: Vec<Tuple<'a>> },
    /// A reader macro and its argument, printed with nothing between them.
    Prefixed { width: usize, prefix: &'a str, inner: Box<Tuple<'a>> },
}

impl Tuple<'_> {
    fn width(&self) -> usize {
        match self {
            Tuple::Flat { width, .. }
            | Tuple::Wide { width, .. }
            | Tuple::Indent { width, .. }
            | Tuple::Prefixed { width, .. } => *width,
        }
    }
}

/// The widest of a column, or `floor` when it is empty.
///
/// `floor` is -1 where the caller is about to add a space that will not be
/// there if the column is empty, which is how `(head)` comes out right.
fn max_width(cols: &[Tuple], floor: isize) -> isize {
    cols.iter().map(|t| t.width() as isize).max().unwrap_or(floor)
}

/// Build the tuple for `x`, to be printed in a column `width` wide, with
/// `rpc` close parens due after it on the same line.
///
/// `rpc` is ACL2's "right paren count", and it is not a detail: in `(f (g (h
/// x)) u)` the `x` will be followed by two close parens whatever happens, so
/// it cannot have the full width. Getting this wrong shifts every boundary by
/// the nesting depth.
fn ppr1<'a>(x: &'a Node, width: usize, rpc: usize) -> Tuple<'a> {
    let sz = x.flat + rpc;
    let flat = || Tuple::Flat { width: sz, items: vec![Item::Obj(x)] };
    // An atom prints on one line however long it is -- there is nowhere to
    // break it. Everything else prints flat only if it fits the page *and*
    // stays inside the flat margin.
    if x.atom_like() || (sz <= width && sz <= FLAT_RIGHT_MARGIN) {
        return flat();
    }
    match &x.kind {
        Kind::Atom(_) => flat(),
        Kind::Prefix(p, inner) => {
            let plen = p.chars().count();
            let t = ppr1(inner, width.saturating_sub(plen), rpc);
            Tuple::Prefixed { width: plen + t.width(), prefix: p, inner: Box::new(t) }
        }
        Kind::List(items, tail) => {
            let Some(head) = items.first() else {
                // `()`, or a list that is nothing but a dotted tail: no head
                // to lay anything out under.
                return flat();
            };
            let width1 = width.saturating_sub(1);
            let alone = items.len() == 1 && tail.is_none();
            let head_t = ppr1(head, width1, if alone { rpc + 1 } else { 0 });
            // A head that is not an atom -- a lambda, say -- cannot have
            // arguments aligned after it, so its width is not a column.
            let hd_sz = head.atom_like().then(|| head_t.width() as isize);
            let cols = ppr1_lst(&items[1..], tail.as_deref(), width1, rpc + 1);
            let maximum = match hd_sz {
                // -1 stands for the space before the arguments that will not
                // be printed if there are none.
                Some(_) => max_width(&cols, -1),
                None => max_width(&cols, 0).max(head_t.width() as isize),
            };
            let head = Box::new(head_t);
            match hd_sz {
                None => Tuple::Indent {
                    width: (1 + maximum).max(0) as usize,
                    indent: 1,
                    head,
                    cols,
                },
                // Room for an open paren, the head, a space, and the widest
                // argument: align the arguments after the head.
                Some(h) if h + 2 + maximum <= width as isize => {
                    Tuple::Wide { width: (h + 2 + maximum).max(0) as usize, head, cols }
                }
                // No room to align, but room to indent. Indent so the widest
                // argument would end on the right margin -- ACL2's exactness
                // -- except that regular columns read better than ragged
                // ones, so give up the white space past five.
                Some(_) if maximum < width as isize => {
                    let i = 5.min(width as isize - maximum).max(1) as usize;
                    Tuple::Indent {
                        width: maximum.max(0) as usize + i,
                        indent: i,
                        head,
                        cols,
                    }
                }
                Some(_) => Tuple::Indent {
                    width: (1 + maximum).max(0) as usize,
                    indent: 1,
                    head,
                    cols,
                },
            }
        }
    }
}

/// Build the column of rows for a list's arguments.
///
/// Built from the bottom up, which is the whole of ACL2's line-packing: each
/// argument either joins the row below it or starts a new one, and only an
/// atom-like argument may join. That single condition is what keeps a
/// parenthesized argument off a line with anything else -- it can never be
/// the thing that joins a row, so it always starts one.
///
/// Iterative rather than recursive: a `defines` clique's scheme has argument
/// lists thousands long, and this recursing on list length would meet the
/// stack long before the term did.
fn ppr1_lst<'a>(
    items: &'a [Node],
    tail: Option<&'a Node>,
    width: usize,
    rpc: usize,
) -> Vec<Tuple<'a>> {
    // Held in reverse print order while it is built, so the row being merged
    // into is the last element rather than the first.
    let mut rows: Vec<Tuple<'a>> = Vec::new();
    let rest: &[Node] = match tail {
        Some(t) => {
            // The dot merges onto the tail's row if there is room for it, and
            // otherwise gets a line of its own.
            rows.push(ppr1(t, width, rpc));
            push_row(&mut rows, Tuple::Flat { width: 1, items: vec![Item::Dot] }, width);
            items
        }
        None => match items.split_last() {
            // The last argument is followed by this list's own close parens,
            // so it is the one that inherits `rpc`.
            Some((last, init)) => {
                rows.push(ppr1(last, width, rpc));
                init
            }
            None => return Vec::new(),
        },
    };
    for item in rest.iter().rev() {
        push_row(&mut rows, ppr1(item, width, 0), width);
    }
    rows.reverse();
    rows
}

/// Add `x` above `rows`, either by joining the row it would sit on or by
/// starting a new one. ACL2's `cons-ppr1`.
///
/// The default is always to start a new row. Joining needs all of: `x` prints
/// atom-like, the row it would join is itself flat, and the result stays
/// inside *both* the page width and the flat margin. The second bound is the
/// one that fires on a half-empty page.
fn push_row<'a>(rows: &mut Vec<Tuple<'a>>, x: Tuple<'a>, width: usize) {
    if let Tuple::Flat { width: xw, items: xitems } = &x
        && xitems.len() == 1
        && xitems[0].atom_like()
        && let Some(Tuple::Flat { width: rw, items: ritems }) = rows.last()
    {
        let n = xw + 1 + rw;
        if n <= FLAT_RIGHT_MARGIN && n <= width {
            let mut merged = xitems.clone();
            merged.extend_from_slice(ritems);
            let last = rows.len() - 1;
            rows[last] = Tuple::Flat { width: n, items: merged };
            return;
        }
    }
    rows.push(x);
}

// ---- Pass two: printing ------------------------------------------------

fn ppr2(x: &Tuple, col: usize, out: &mut String) {
    match x {
        Tuple::Flat { items, .. } => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                item.write_flat(out);
            }
        }
        Tuple::Prefixed { prefix, inner, .. } => {
            out.push_str(prefix);
            ppr2(inner, col + prefix.chars().count(), out);
        }
        Tuple::Wide { head, cols, .. } => {
            out.push('(');
            ppr2(head, col + 1, out);
            let at = col + 1 + head.width();
            ppr2_column(cols, at, at + 1, out);
            out.push(')');
        }
        Tuple::Indent { indent, head, cols, .. } => {
            out.push('(');
            ppr2(head, col + indent, out);
            if !cols.is_empty() {
                out.push('\n');
                ppr2_column(cols, 0, col + indent, out);
            }
            out.push(')');
        }
    }
}

/// Print `rows` in a column at `col`, with the print head currently at `loc`.
///
/// A row that would have to move left gets one space instead: the column is
/// where rows *want* to be, not a promise the line above will allow.
fn ppr2_column(rows: &[Tuple], mut loc: usize, col: usize, out: &mut String) {
    for (i, row) in rows.iter().enumerate() {
        let pad = if col > loc { col - loc } else { 1 };
        out.push_str(&" ".repeat(pad));
        ppr2(row, if col > loc { col } else { loc + 1 }, out);
        if i + 1 < rows.len() {
            out.push('\n');
            loc = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything this cannot read comes back untouched, byte for byte.
    #[test]
    fn unreadable_text_is_returned_as_it_arrived() {
        for s in [
            "(f x",                       // unbalanced
            "(f x) (g y)",                // more than one term
            "(f \"unterminated)",         // unterminated string
            "(f |unterminated)",          // unterminated bar symbol
            ")",                          // no term at all
            "'",                          // a prefix with nothing after it
            "",
        ] {
            assert_eq!(pretty(s, 30), s, "{s:?}");
        }
        // Nesting past the depth guard, rather than a blown stack.
        let deep = format!("{}x{}", "(".repeat(MAX_DEPTH + 5), ")".repeat(MAX_DEPTH + 5));
        assert_eq!(pretty(&deep, 30), deep);
    }

    /// A reader macro binds to what follows with nothing between. Treating
    /// `'` as an ordinary atom inserted a space and so changed the term.
    #[test]
    fn reader_macros_keep_their_form() {
        for s in ["(equal x '(1 2))", "(list `(a ,b ,@c) #'f)", "(f '(a . b))"] {
            assert_eq!(pretty(s, 60), s, "{s:?}");
        }
        // And when it has to break, the prefix still leads its term.
        let out = pretty("(equal xxxxxxxxxxxxxxxxxxxx '(aaaaaaaaaa bbbbbbbbbb cccccccccc))", 40);
        assert!(out.contains("'("), "{out}");
        assert!(!out.contains("' ("), "{out}");
    }

    /// Character literals and quoted symbols hide delimiters.
    #[test]
    fn quoting_forms_are_opaque_to_the_reader() {
        for s in ["(equal c #\\()", "(equal c #\\))", "(equal s \"a\\\"b(c\")", "(equal |a b| x)"] {
            assert_eq!(pretty(s, 40), s, "{s:?}");
        }
    }

    /// The flat margin is not the page width: it fires with the page half
    /// empty. Measured against ACL2 at width 100, the boundary for `(f a b)`
    /// falls between a whole-form width of 43 and 44 -- because the last
    /// argument's width includes the close paren that will follow it.
    #[test]
    fn the_flat_margin_breaks_a_line_the_page_had_room_for() {
        let fits = format!("(f {} {})", "a".repeat(19), "b".repeat(19));
        assert_eq!(fits.chars().count(), 43);
        assert_eq!(pretty(&fits, 100), fits);

        let over = format!("(f {} {})", "a".repeat(19), "b".repeat(20));
        assert_eq!(over.chars().count(), 44);
        assert_eq!(pretty(&over, 100), format!("(f {}\n   {})", "a".repeat(19), "b".repeat(20)));
    }

    /// A parenthesized argument never joins a row, so it starts one -- and
    /// atoms above it then pile onto its row. This is ACL2's own example.
    #[test]
    fn a_parenthesized_argument_starts_its_own_row() {
        // Checked against ACL2 at this width; `(foo (bar a) b c d)` is the
        // essay's own example, but it fits inside `MIN_WIDTH` and so never
        // reaches the rule.
        assert_eq!(pretty("(foo (bar aaaaa) b c d)", 20), "(foo (bar aaaaa)\n     b c d)");
        // The other order: `dddd` joins the row `(bar a)` started, which is
        // the same rule seen from the other side rather than an exception.
        let out = pretty("(foo bbbb cccc dddd (bar aaaa) eeee)", 20);
        assert!(out.contains("dddd (bar aaaa)"), "{out}");
    }

    /// No head is forced to break: a conjunction that fits, fits.
    #[test]
    fn nothing_breaks_a_form_that_fits() {
        assert_eq!(pretty("(and (natp x) (natp y))", 60), "(and (natp x) (natp y))");
        assert_eq!(pretty("(implies (natp x) (natp y))", 60), "(implies (natp x) (natp y))");
    }

    /// A head too long to align under puts its arguments on their own lines,
    /// indented so the widest would end on the right margin -- capped at five,
    /// because regular columns read better than ragged ones.
    #[test]
    fn a_long_head_indents_rather_than_aligns() {
        let out = pretty("(some-very-long-function-name aaaa bbbb cccc)", 30);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "(some-very-long-function-name");
        for l in &lines[1..] {
            let indent = l.len() - l.trim_start().len();
            assert!((1..=5).contains(&indent), "{out}");
        }
    }

    /// Indentation is bounded by the algorithm, not by a cap bolted on: the
    /// aligned shape is only chosen when it fits, and the other shape indents
    /// by at most five. So a deeply nested term cannot march off the page,
    /// and the output stays proportional to the input.
    #[test]
    fn layout_stays_proportional_on_a_deeply_nested_term() {
        let mut t = String::from("(equal x y)");
        for i in 0..800 {
            t = format!("(and {t} (implies (natp x{i}) (equal (f x{i}) (g x{i}))))");
        }
        let out = pretty(&t, 60);
        assert!(
            out.len() < t.len() * 6,
            "{}x expansion on {} bytes",
            out.len() / t.len(),
            t.len()
        );
        let widest = out.lines().map(|l| l.len() - l.trim_start().len()).max().unwrap();
        assert!(widest <= 60, "indent ran to {widest}");
    }

    /// A dotted tail keeps its dot, and the dot rides the tail's line when
    /// there is room for it.
    #[test]
    fn a_dotted_list_keeps_its_dot() {
        assert_eq!(pretty("(a . b)", 40), "(a . b)");
        // Both checked against ACL2. The dot rides the tail's line when there
        // is room for it, and takes a line of its own when there is not.
        let dotted = format!("(cons {} . {})", "a".repeat(20), "b".repeat(20));
        assert_eq!(
            pretty(&dotted, 60),
            format!("(cons {}\n      . {})", "a".repeat(20), "b".repeat(20))
        );
        assert_eq!(
            pretty(&dotted, 20),
            format!("(cons\n {}\n .\n {})", "a".repeat(20), "b".repeat(20))
        );
    }

    /// Laying out an already laid-out term changes nothing further.
    #[test]
    fn the_layout_is_stable() {
        for s in [
            "(equal x '(1 2))",
            "(and (natp x) (natp y))",
            "(implies (and (natp x) (natp y) (< x y)) (equal (foo x y) (bar y x)))",
            "(some-very-long-function-name aaaa bbbb cccc)",
            "(foo (bar a) b c d)",
        ] {
            for w in [20, 30, 40, 77] {
                let once = pretty(s, w);
                assert_eq!(pretty(&once, w), once, "{s:?} at {w}");
            }
        }
    }

    /// Whatever the width, the text still says what it said: dropping the
    /// layout back out must give the term it started from.
    #[test]
    fn laying_out_never_changes_the_term() {
        let flatten = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        for s in [
            "(implies (and (natp x) (natp y) (< x y)) (equal (foo x y) (bar y x)))",
            "(equal x '(1 2))",
            "(f (g (h (i (j k)))) (l m n) #\\( \"a b\" |c d|)",
            "(cons a . b)",
        ] {
            for w in [20, 25, 33, 60, 200] {
                assert_eq!(flatten(&pretty(s, w)), flatten(s), "{s:?} at {w}");
            }
        }
    }

    /// A page narrower than anything real is treated as the floor rather than
    /// producing a column of one character per line.
    #[test]
    fn an_absurd_width_is_floored() {
        assert_eq!(pretty("(equal (foo aaaa bbbb) (bar cccc dddd))", 1), pretty("(equal (foo aaaa bbbb) (bar cccc dddd))", MIN_WIDTH));
    }
}
