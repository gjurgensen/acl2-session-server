// Re-flow an ACL2 term to the width it is actually being read at.
//
// ACL2 pretty-prints for a 77-column terminal. The panel is a side pane and
// is usually much narrower, so a term that fitted on one line for ACL2
// arrives as one long line here and gets soft-wrapped by CSS at whatever
// column the pane happens to be -- mid-argument, with no indentation to show
// what belongs to what. This lays it out again at the real width.
//
// Deliberately in the client rather than in the core or the daemon: the
// width is a property of the window, nobody else knows it, and it changes
// when the pane is dragged. It is also the only place that can re-flow a
// term from *any* source uniformly -- checkpoints from a probe, a scheme
// from the log, a statement from a hover -- and the only one that keeps
// working while a proof is grinding.
//
// Total, like the induction parser: anything that does not parse comes back
// exactly as it arrived. A term this cannot read is a term ACL2 printed and
// we would rather show verbatim than mangle.

/** A parsed term: an atom's text, or a list of nodes. */
type Node = string | Node[];

/**
 * Heads whose arguments always go one per line, whatever the width.
 *
 * The one rule here that is a style choice rather than a consequence of the
 * width, and deliberately unconditional: a goal is read as its structure,
 * not as text that happens to fit. `implies` breaks after the hypotheses so
 * the conclusion starts a line of its own; a conjunction puts each conjunct
 * on its own line so they can be counted and compared down the page. That a
 * short one would fit is not a reason to run them together.
 *
 * Extending this is a one-line change; the binding forms are the obvious
 * candidates if it ever feels short.
 */
const BREAK_HEADS = new Set(['implies', 'and', 'or', 'iff', 'cond', 'case']);

/** Longer heads stop earning an aligned column; their arguments go under. */
const ALIGN_LIMIT = 12;

export function pretty(term: string, cols: number): string {
  const node = parse(term);
  if (node === undefined) {
    return term;
  }
  return render(node, 0, Math.max(20, cols));
}

// ---- Reading -----------------------------------------------------------

/**
 * Parse one term, or give up.
 *
 * Gives up on anything left over, on unbalanced parens, and on anything it
 * does not recognise -- all of which mean "show it as ACL2 printed it".
 */
function parse(src: string): Node | undefined {
  const toks = lex(src);
  if (!toks) {
    return undefined;
  }
  let i = 0;
  const node = (): Node | undefined => {
    const t = toks[i];
    if (t === undefined) {
      return undefined;
    }
    if (t === ')') {
      return undefined;
    }
    if (t !== '(') {
      i++;
      return t;
    }
    i++;
    const out: Node[] = [];
    for (;;) {
      if (i >= toks.length) {
        return undefined; // unbalanced
      }
      if (toks[i] === ')') {
        i++;
        return out;
      }
      const child = node();
      if (child === undefined) {
        return undefined;
      }
      out.push(child);
    }
  };
  const first = node();
  return first !== undefined && i === toks.length ? first : undefined;
}

/**
 * Split into parens and atoms.
 *
 * Strings, `#\(` character literals and `|bar quoted|` symbols each hide
 * characters that must not be read as structure -- the same set the console's
 * balance check and the core's term reader have to know about.
 */
function lex(src: string): string[] | undefined {
  const out: string[] = [];
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    if (/\s/.test(c)) {
      i++;
      continue;
    }
    if (c === '(' || c === ')') {
      out.push(c);
      i++;
      continue;
    }
    // An atom runs to the next delimiter, with the quoting forms opaque.
    const start = i;
    while (i < src.length && !/[\s()]/.test(src[i])) {
      if (src[i] === '"' || src[i] === '|') {
        const q = src[i];
        i++;
        while (i < src.length && src[i] !== q) {
          i += src[i] === '\\' ? 2 : 1;
        }
        if (i >= src.length) {
          return undefined; // unterminated
        }
        i++;
      } else if (src[i] === '#' && src[i + 1] === '\\') {
        i += 3;
      } else {
        i++;
      }
    }
    if (i === start) {
      return undefined;
    }
    out.push(src.slice(start, i));
  }
  return out;
}

// ---- Laying out --------------------------------------------------------

/** The one-line form, ignoring width. */
function flat(n: Node): string {
  return typeof n === 'string' ? n : `(${n.map(flat).join(' ')})`;
}

/**
 * Whether this form is laid out down the page whatever the width.
 *
 * Propagates upwards: a form containing one cannot itself be flat, or the
 * result is a broken conjunction sitting inside an unbroken parent, which
 * reads worse than either.
 */
function mustBreak(n: Node): boolean {
  if (typeof n === 'string') {
    return false;
  }
  const [head, ...args] = n;
  if (
    typeof head === 'string' &&
    BREAK_HEADS.has(head.toLowerCase().replace(/^acl2::/, '')) &&
    args.length >= 2
  ) {
    return true;
  }
  return n.some(mustBreak);
}

/**
 * Lay `n` out starting at column `col`.
 *
 * The first line carries no indentation -- the caller has already placed it
 * -- while every later line is indented absolutely, so a nested call's output
 * can be dropped in as it stands.
 */
function render(n: Node, col: number, cols: number): string {
  const one = flat(n);
  if (col + one.length <= cols && !mustBreak(n)) {
    return one;
  }
  if (typeof n === 'string' || n.length === 0) {
    return one; // an atom, or `()`: nothing to break
  }
  const [head, ...args] = n;
  const headText = flat(head);
  // Arguments line up after "(head ", which is what makes a broken form
  // readable. A head that is itself a form, or a long one, would push that
  // column so far right that the arguments have no room, so those indent by
  // one instead.
  const aligned = typeof head === 'string' && headText.length <= ALIGN_LIMIT;
  const argCol = aligned ? col + headText.length + 2 : col + 1;
  if (args.length === 0) {
    return `(${headText})`;
  }
  const pad = ' '.repeat(argCol);
  const parts = args.map((a) => render(a, argCol, cols));
  // The first argument shares the head's line when it is aligned there;
  // otherwise every argument starts on its own.
  return aligned
    ? `(${headText} ${parts[0]}${parts.slice(1).map((p) => `\n${pad}${p}`).join('')})`
    : `(${headText}${parts.map((p) => `\n${pad}${p}`).join('')})`;
}
