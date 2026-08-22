// Lisp indentation, computed from structure rather than from regexes.
//
// VSCode's own auto-indent is driven by `indentationRules` in a language
// configuration, which are regexes matched against the previous line. That
// model cannot express Lisp indentation at any quality: where a line belongs
// depends on the paren nesting above it, on the head symbol of the enclosing
// form, and on the column the head's first argument happens to sit in --
// none of which a regex over one line can see. So the rules are computed
// here and delivered through the formatting providers instead, which is the
// one route VSCode leaves open for a language that needs to think.
//
// Kept free of `vscode`, like `positions.ts` and `policy.ts`, for two
// reasons. The obvious one is that it can then be tested without an editor,
// against whole books. The other is that indentation must work in a file
// with no session: the daemon is spawned lazily and may not be installed at
// all, and a person typing into a `.lisp` file is owed a working Tab either
// way. Nothing in here may reach for the daemon.
//
// The rule set is not invented. It is Emacs' `lisp-indent-function` model --
// the one every ACL2 book in the community checkout was indented by -- with
// the community's own tables transcribed into `FORMS`. See that constant for
// where each entry came from.

/**
 * How a form's arguments are indented, in Emacs' `lisp-indent-function`
 * vocabulary.
 *
 * `'defun` means "like a definition": the body sits at the open paren's
 * column plus `bodyIndent`, however many arguments the head line carries. A
 * number `k` means the form has `k` distinguished arguments, indented deeper
 * than the body that follows them -- `(mv-let (a b) <distinguished> body)`.
 */
export type IndentSpec = 'defun' | number;

export interface IndentOptions {
  /** Extra entries, and overrides, for {@link FORMS}. */
  forms?: Record<string, IndentSpec>;
  /** Emacs' `lisp-body-indent`: how far a body sits inside its form. */
  bodyIndent?: number;
  /** How much deeper a distinguished argument sits than the body. */
  distinguished?: number;
  /**
   * Where the first argument of a plain function call goes when the head is
   * alone on its line.
   *
   * Emacs' own answer is the open paren's column plus one, so the argument
   * lines up just inside the paren. `acl2-indent.el` makes this adjustable
   * and defaults it to one more than that; its `indent-first-function-arg`
   * is this number minus one.
   *
   * Emacs' bare default is the right one to keep. Widening it to 2 looks
   * attractive from the `(must-fail` / `(must-succeed*` wrappers, whose
   * argument does start two columns in -- but those are macros whose body is
   * a body, and naming them as such is the fix. Applied globally the wider
   * indent costs eighteen points of corpus agreement, because it also moves
   * every `(xdoc::p "..."` whose string begins on the next line.
   */
  firstArg?: number;
  /**
   * Treat an unknown head beginning `def` as a definition.
   *
   * ACL2 is written in macros, and a book's own definers outnumber the ones
   * any table can name. `acl2-indent.el` settled on this heuristic years ago
   * (`indent-def-beginning-functions-like-defun`) and the books bear it out.
   */
  defPrefix?: boolean;
}

const DEFAULTS: Required<Omit<IndentOptions, 'forms'>> = {
  bodyIndent: 2,
  distinguished: 2,
  firstArg: 1,
  defPrefix: true,
};

/**
 * The indent table, transcribed rather than invented.
 *
 * Three sources, in increasing order of specificity, merged here:
 *
 *   - Emacs' own `lisp-mode` defaults, for the forms Common Lisp and ACL2
 *     share (`let`, `cond`, `when`, ...). These are what a book gets when
 *     nobody has loaded anything ACL2-specific, so they are the floor.
 *   - `emacs/emacs-acl2.el` in the ACL2 sources, which is what the ACL2
 *     developers load. Twenty-four `put` forms, mostly `with-...` wrappers.
 *   - `books/interface/emacs/acl2-indent.el` (Stephen Westfold), the richest
 *     of the three at sixty-four entries, and the only one that knows the
 *     Kestrel definers -- `defiso`, `defmapping`, `defbyte`.
 *
 * Where they disagree the most specific wins, which is how `encapsulate`
 * ends up at 1 rather than `defun`. Those disagreements are real and few;
 * `indent.mjs` measures each of them against the books rather than settling
 * it by argument.
 */
export const FORMS: Record<string, IndentSpec> = {
  // Emacs `lisp-mode`, for what the dialects share.
  //
  // `if` is the one entry the three sources disagree on outright:
  // `acl2-mode.el` sets it to nil (a plain call, arguments aligned under the
  // test), while `books/doc/practices.lisp` tells readers to
  // `(put 'if 'common-lisp-indent-function 2)`. The books settle it -- 2 is
  // what they are written in, and assuming nil costs about a point of
  // agreement across the corpus on its own.
  if: 2,
  lambda: 'defun',
  let: 1,
  'let*': 1,
  flet: 1,
  labels: 1,
  when: 1,
  unless: 1,
  while: 1,
  cond: 0,
  progn: 0,
  prog1: 1,
  prog2: 2,
  catch: 1,
  'multiple-value-bind': 2,
  'destructuring-bind': 2,

  // emacs/emacs-acl2.el.
  case: 'defun',
  'case!': 'defun',
  'case-match': 'defun',
  dolist: 'defun',
  'er@par': 'defun',
  'warning$@par': 'defun',
  'er-let*': 1,
  'with-acl2-channels-bound': 'defun',
  'with-fast-alist': 'defun',
  'with-fast-alists': 'defun',
  'with-global-stobj': 'defun',
  'with-local-stobj': 'defun',
  'with-open-file': 'defun',
  'with-output': 'defun',
  'with-output!': 'defun',
  'with-output-to': 'defun',
  'with-stdout': 'defun',
  'pattern-match': 'defun',
  'pattern-match-list': 'defun',
  'verify-termination': 'defun',

  // books/interface/emacs/acl2-indent.el.
  'b*': 1,
  'mv-let': 1,
  encapsulate: 1,
  'verify-guards': 1,
  defun: 'defun',
  'defun$': 'defun',
  defund: 'defun',
  'defun-sk': 'defun',
  'defund-sk': 'defun',
  deflist: 'defun',
  defprojection: 'defun',
  deftheory: 'defun',
  theorem: 'defun',
  axiom: 'defun',
  simplify: 'defun',
  spec: 'defun',
  op: 'defun',
  defthm: 1,
  defthmd: 1,
  'defthm-std': 1,
  defaxiom: 1,
  defchoose: 2,
  defconst: 1,
  defconstant: 1,
  defparameter: 1,
  defvar: 1,
  defpkg: 1,
  defstobj: 1,
  defrec: 1,
  defaggregate: 1,
  defxdoc: 1,
  'defxdoc+': 1,
  defbyte: 1,
  defubytelist: 1,
  deflist2: 1,
  defset: 1,
  defoset: 1,
  defsum: 1,
  defresult: 1,
  defsubtype: 1,
  definj: 1,
  defsurj: 1,
  defisar: 1,
  defiso: 5,
  defmapping: 1,
  'propagate-iso': 1,
  the: 1,
  'er-soft': 3,
  'er-soft+': 3,
  'define-skeleton': 3,
  'define-abbrev-table': 1,

  // The testing macros. Not in any of the three tables, because they live in
  // the books rather than in ACL2 proper -- but a book's test file is mostly
  // made of them, and their argument is a form to be indented as a body.
  'must-succeed*': 0,
  'must-eval-to': 1,
  'must-eval-to-t': 0,
  'must-be-redundant': 0,
  'must-prove': 0,
  'must-not-prove': 0,
  'assert!': 0,
  'assert-equal': 0,
  'must-not-be-tables': 0,
};

/** An open delimiter still unclosed at some point in the text. */
interface Open {
  /** Offset of the delimiter itself. */
  offset: number;
  /** Its column, in characters from the start of its line. */
  column: number;
}

const isSpace = (c: string): boolean => c === ' ' || c === '\t' || c === '\n' || c === '\r';

/**
 * Whether a character can be part of a symbol.
 *
 * Deliberately permissive: ACL2 symbols carry `$`, `*`, `+`, `<`, `!` and
 * package colons, and the only thing that reliably is *not* part of one is
 * whitespace, a delimiter, or a reader character.
 */
const isAtom = (c: string): boolean =>
  !isSpace(c) && !'()[]{}";\'`,'.includes(c);

/**
 * Skip whatever non-datum thing starts at `i`, or return `i` unchanged.
 *
 * The four things that hide parens from a naive scan -- strings, line
 * comments, block comments, and character literals -- plus `|bar symbols|`,
 * which can hide anything at all. Getting these wrong is not a cosmetic
 * matter: a `;` inside a string that swallowed the rest of a line would
 * silently shift every form below it.
 */
function skipNonDatum(text: string, i: number): number {
  const c = text[i];
  if (c === ';') {
    const nl = text.indexOf('\n', i);
    return nl === -1 ? text.length : nl;
  }
  if (c === '"') {
    let j = i + 1;
    while (j < text.length) {
      if (text[j] === '\\') {
        j += 2;
        continue;
      }
      if (text[j] === '"') {
        return j + 1;
      }
      j++;
    }
    return text.length;
  }
  if (c === '|') {
    const end = text.indexOf('|', i + 1);
    return end === -1 ? text.length : end + 1;
  }
  if (c === '#') {
    const n = text[i + 1];
    if (n === '|') {
      // Nested, per the standard: `#| a #| b |# c |#` closes once, not twice.
      let depth = 1;
      let j = i + 2;
      while (j < text.length && depth > 0) {
        if (text[j] === '#' && text[j + 1] === '|') {
          depth++;
          j += 2;
        } else if (text[j] === '|' && text[j + 1] === '#') {
          depth--;
          j += 2;
        } else {
          j++;
        }
      }
      return j;
    }
    if (n === '\\') {
      // `#\(` is a character, not an open paren. One character always, then
      // any name that follows it -- `#\Space`, `#\Newline`.
      let j = i + 3;
      while (j < text.length && isAtom(text[j])) {
        j++;
      }
      return j;
    }
  }
  return i;
}

/**
 * The delimiters still open at `limit`, outermost first.
 *
 * A plain left-to-right scan, which is the only honest way: whether a `(` is
 * a paren at all depends on everything before it. What keeps that affordable
 * is not scanning from the top of the file -- see {@link topLevelStarts}.
 */
export function openStack(text: string, from: number, limit: number, lineStart: number): Open[] {
  const stack: Open[] = [];
  let bol = lineStart;
  let i = from;
  while (i < limit) {
    const c = text[i];
    if (c === '\n') {
      bol = i + 1;
      i++;
      continue;
    }
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      // A skipped run can span lines -- a long string, a block comment --
      // and the column of everything after it depends on where it ended.
      for (let k = i; k < skipped && k < limit; k++) {
        if (text[k] === '\n') {
          bol = k + 1;
        }
      }
      i = skipped;
      continue;
    }
    if (c === '(' || c === '[' || c === '{') {
      stack.push({ offset: i, column: i - bol });
    } else if (c === ')' || c === ']' || c === '}') {
      // An unmatched close is not an error to report from here. The file is
      // being edited; it is unbalanced most of the time.
      stack.pop();
    }
    i++;
  }
  return stack;
}

/**
 * Offsets where a top-level form begins.
 *
 * The point of this is bounding the work. Indentation only ever needs the
 * enclosing top-level form, and re-scanning an 8000-line book on every
 * keystroke is not affordable -- `kestrel/c/syntax/validator.lisp` has a
 * single `defines` spanning five thousand lines. Scanning from the nearest
 * start above the cursor makes the cost proportional to one form.
 *
 * A `(` in column zero is the usual signal, but only the usual one: the same
 * shape appears inside a multi-line string. So this is a real scan, and the
 * result is worth caching against a document version.
 */
export function topLevelStarts(text: string): number[] {
  const starts: number[] = [];
  let depth = 0;
  let i = 0;
  while (i < text.length) {
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      i = skipped;
      continue;
    }
    const c = text[i];
    if (c === '(' || c === '[' || c === '{') {
      if (depth === 0) {
        starts.push(i);
      }
      depth++;
    } else if (c === ')' || c === ']' || c === '}') {
      depth = Math.max(0, depth - 1);
    }
    i++;
  }
  return starts;
}

/** A datum's extent, and enough about it to indent against. */
interface Datum {
  start: number;
  end: number;
  column: number;
  /** The text, when it is an atom; absent for a list. */
  atom?: string;
}

/**
 * Read the datum starting at or after `i`, stopping before `limit`.
 *
 * Prefix characters belong to the datum they precede -- `'(a b)` indents as
 * the list does, one column right -- which is what Emacs' own
 * `backward-prefix-chars` is for.
 */
function readDatum(text: string, i: number, limit: number, bolOf: (o: number) => number): Datum | null {
  let j = i;
  while (j < limit) {
    const c = text[j];
    if (isSpace(c)) {
      j++;
      continue;
    }
    // A comment is not a datum; skip it and keep looking.
    if (c === ';' || (c === '#' && text[j + 1] === '|')) {
      j = skipNonDatum(text, j);
      continue;
    }
    break;
  }
  if (j >= limit) {
    return null;
  }
  const start = j;
  // Prefixes first, so `,@foo` and `#'f` come out as one datum.
  while (j < limit && ("'`,".includes(text[j]) || (text[j] === '#' && text[j + 1] === "'"))) {
    j += text[j] === '#' ? 2 : 1;
  }
  const c = text[j];
  if (c === '(' || c === '[' || c === '{') {
    let depth = 0;
    while (j < text.length) {
      const skipped = skipNonDatum(text, j);
      if (skipped !== j) {
        j = skipped;
        continue;
      }
      const d = text[j];
      if (d === '(' || d === '[' || d === '{') {
        depth++;
      } else if (d === ')' || d === ']' || d === '}') {
        depth--;
        if (depth === 0) {
          j++;
          break;
        }
      }
      j++;
    }
    return { start, end: j, column: start - bolOf(start) };
  }
  const skipped = skipNonDatum(text, j);
  if (skipped !== j) {
    // A string or a `|bar symbol|` -- a datum, but not one with a head name.
    return { start, end: skipped, column: start - bolOf(start) };
  }
  let k = j;
  while (k < text.length && isAtom(text[k])) {
    k++;
  }
  if (k === j) {
    k++;
  }
  return { start, end: k, column: start - bolOf(start), atom: text.slice(j, k) };
}

/**
 * The head symbol's lookup key.
 *
 * The package qualifier is dropped so that `std::define` and `define` share
 * an entry -- with one exception, kept from `acl2-indent.el`: a name already
 * beginning `def` keeps whatever it has, because the `def` heuristic is
 * about the name a person reads, not the package it lives in.
 */
export function lookupKey(atom: string): string {
  const lower = atom.toLowerCase();
  if (lower.startsWith('def')) {
    return lower;
  }
  const colon = lower.lastIndexOf(':');
  return colon === -1 || colon === lower.length - 1 ? lower : lower.slice(colon + 1);
}

/**
 * How far the line beginning at `lineStart` should be indented.
 *
 * `from` bounds the scan; pass the enclosing top-level form's start, or 0.
 * The answer is a column, and it is the caller's business to decide whether
 * a line inside a string or a block comment should be moved at all -- see
 * {@link reindent}, which does not.
 */
export function indentFor(
  text: string,
  lineStart: number,
  from = 0,
  fromLine = 0,
  opts: IndentOptions = {},
  open0?: Open[]
): number {
  const o = { ...DEFAULTS, ...opts };
  const forms = opts.forms ? { ...FORMS, ...opts.forms } : FORMS;
  // A caller walking the whole buffer already knows the stack and has kept
  // it up to date line by line; re-deriving it here would make a
  // whole-file pass quadratic. `kestrel/c/syntax/validator.lisp`, whose
  // largest form spans five thousand lines, took 9.8 s that way.
  const stack = open0 ?? openStack(text, from, lineStart, fromLine);
  const open = stack[stack.length - 1];
  if (!open) {
    // Top level. Every form in every book starts in column zero, and the
    // one thing worse than not indenting is indenting these.
    return 0;
  }
  const bolOf = (offset: number): number => {
    const nl = text.lastIndexOf('\n', offset - 1);
    return nl + 1;
  };
  const head = readDatum(text, open.offset + 1, lineStart, bolOf);
  if (!head) {
    // `(` with nothing after it yet, which is what a half-typed form looks
    // like. One column in, so the first thing typed lands where its
    // siblings will.
    return open.column + 1;
  }
  if (head.atom === undefined) {
    // The first element is a list or a string, so this is data rather than a
    // call -- a `let` binding list, a `cond` clause, an alist. Align under
    // it, which is what makes columns of clauses line up.
    return head.column;
  }

  const key = lookupKey(head.atom);
  const spec = forms[key];
  // The `def` heuristic is what makes this workable at all: a book's own
  // definers outnumber every table's entries, and turning it off costs
  // thirty-eight points of corpus agreement. The obvious companion rule --
  // treat `..._case` the same, for fty's generated case macros -- was tried
  // and *costs* three points, because those are written inline as
  // expressions and take ordinary call alignment. It is not here for that
  // reason, and should not be added back without the measurement.
  const effective: IndentSpec | undefined =
    spec ?? (o.defPrefix && key.startsWith('def') ? 'defun' : undefined);

  const body = open.column + o.bodyIndent;
  if (effective === 'defun') {
    return body;
  }
  if (typeof effective === 'number') {
    // Count the arguments that finish before this line: that is the index of
    // the one starting here. Distinguished arguments sit deeper than the
    // body, so that `(mv-let (a b) (f x) body)` shows at a glance which of
    // its parts is which.
    // Stop as soon as the answer is settled. Only the comparison against
    // `effective` matters, and `effective` is a small number -- so this walks
    // a handful of arguments rather than every argument of a form that may
    // be thousands of lines long.
    let count = 0;
    let at = head.end;
    while (count < effective) {
      const d = readDatum(text, at, lineStart, bolOf);
      if (!d) {
        break;
      }
      count++;
      at = d.end;
    }
    return count < effective ? body + o.distinguished : body;
  }

  // A plain call. If an argument shares the head's line, everything below
  // lines up under it; that alignment is the single most recognisable thing
  // about how Lisp is written.
  const first = readDatum(text, head.end, lineStart, bolOf);
  if (first && bolOf(first.start) === bolOf(head.start)) {
    return first.column;
  }
  return open.column + o.firstArg;
}

/**
 * Whether a line's indentation is the author's business rather than ours.
 *
 * A comment opened with a single `;` is, by a convention ACL2 has kept since
 * before the books were books: those comments sit where they were typed --
 * very often in column zero, inside a form nested twenty columns deep -- and
 * carry meaning by it. Emacs' own `lisp-mode` would move them to
 * `comment-column`; the corpus says plainly that nobody does. `;;` and
 * `;;;` are ordinary and get the structural indent.
 *
 * Exported because both the whole-buffer pass and the editor's per-line
 * providers have to agree about it, and it is the sort of rule that is worth
 * being able to point at.
 */
export function keepsOwnColumn(lineText: string): boolean {
  const t = lineText.trimStart();
  return t.startsWith(';') && !t.startsWith(';;');
}

/** A line's leading whitespace, in characters. */
function leading(line: string): number {
  let n = 0;
  while (n < line.length && (line[n] === ' ' || line[n] === '\t')) {
    n++;
  }
  return n;
}

/**
 * Lexical state carried from one line to the next.
 *
 * Re-indenting has to know two things at each line: how deep the parens are,
 * so it knows which top-level form it is inside, and whether the line begins
 * inside a string or a block comment, where leading whitespace is content
 * rather than layout. Both are answers about everything above the line, and
 * carrying them forward is what keeps a whole-buffer pass linear instead of
 * rescanning the form above every line.
 */
interface LineState {
  /** The delimiters still open, outermost first. */
  stack: Open[];
  inString: boolean;
  block: number;
}

/**
 * Advance the lexical state across one line.
 *
 * `lineStart` is the line's offset in the text being built, so that an open
 * delimiter can record where it is and what column it sits in -- which is
 * the whole of what indentation needs from the lines above.
 */
function stepLine(line: string, lineStart: number, st: LineState): LineState {
  const stack = st.stack.slice();
  let { inString, block } = st;
  let i = 0;
  while (i < line.length) {
    const c = line[i];
    if (inString) {
      if (c === '\\') {
        i += 2;
        continue;
      }
      if (c === '"') {
        inString = false;
      }
      i++;
      continue;
    }
    if (block > 0) {
      if (c === '#' && line[i + 1] === '|') {
        block++;
        i += 2;
      } else if (c === '|' && line[i + 1] === '#') {
        block--;
        i += 2;
      } else {
        i++;
      }
      continue;
    }
    if (c === ';') {
      break;
    }
    if (c === '"') {
      inString = true;
      i++;
      continue;
    }
    if (c === '#' && line[i + 1] === '|') {
      block++;
      i += 2;
      continue;
    }
    if (c === '#' && line[i + 1] === '\\') {
      i += 3;
      while (i < line.length && isAtom(line[i])) {
        i++;
      }
      continue;
    }
    if (c === '|') {
      // A `|bar symbol|` closing on a later line is not a thing any book
      // does; treating it as line-local keeps the state machine finite.
      const end = line.indexOf('|', i + 1);
      i = end === -1 ? line.length : end + 1;
      continue;
    }
    if (c === '(' || c === '[' || c === '{') {
      stack.push({ offset: lineStart + i, column: i });
    } else if (c === ')' || c === ']' || c === '}') {
      stack.pop();
    }
    i++;
  }
  return { stack, inString, block };
}

/**
 * Re-indent every line.
 *
 * Top down and against the running result, because each line's target
 * depends on the columns of the lines above it: re-indenting against the
 * original would compute every line after the first from stale columns.
 *
 * A line that begins inside a string or a block comment is emitted
 * untouched. Its leading whitespace is part of what the file says -- xdoc
 * documentation strings in the books are full of deliberately aligned prose
 * -- and moving it would be an edit, not a formatting.
 */
export function reindent(text: string, opts: IndentOptions = {}): string {
  const lines = text.split('\n');
  let out = '';
  let st: LineState = { stack: [], inString: false, block: 0 };
  /** Offset in `out` of the top-level form this line is inside. */
  let from = 0;
  for (let n = 0; n < lines.length; n++) {
    const line = lines[n];
    const lineStart = out.length;
    const opaque = st.inString || st.block > 0;
    if (st.stack.length === 0 && !opaque) {
      // Between forms: whatever starts here is the next one, and nothing
      // above it can affect how anything inside it is indented.
      from = lineStart;
    }
    const rest = line.slice(leading(line));
    let emitted: string;
    if (opaque || keepsOwnColumn(line)) {
      emitted = line;
    } else if (rest.length === 0) {
      emitted = '';
    } else {
      const want = indentFor(out, lineStart, from, from, opts, st.stack);
      emitted = ' '.repeat(Math.max(0, want)) + rest;
    }
    out += emitted;
    if (n !== lines.length - 1) {
      out += '\n';
    }
    st = stepLine(emitted, lineStart, st);
  }
  return out;
}

function lineStartOf(text: string, offset: number): number {
  return text.lastIndexOf('\n', offset - 1) + 1;
}

/**
 * Whether `limit` sits inside a string or a block comment.
 *
 * Both are places where leading whitespace is content rather than layout,
 * and moving it changes what the file means.
 */
export function insideOpaque(text: string, from: number, limit: number): boolean {
  let i = from;
  while (i < limit) {
    const skipped = skipNonDatum(text, i);
    if (skipped !== i) {
      if (skipped > limit) {
        // The run swallows the target, so the target is inside it -- unless
        // it is a line comment, which ends at the newline anyway.
        return text[i] !== ';';
      }
      i = skipped;
      continue;
    }
    i++;
  }
  return false;
}
