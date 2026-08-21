/**
 * What the world knows about a name, and how it reads in a hover.
 *
 * Deliberately free of `vscode`, like `protocol.ts` and for the same reason:
 * the rendering is the part with decisions in it, so it is the part that has
 * to be checkable under plain node. The provider that wraps it is glue.
 */

export interface DescribeReply {
  /** False when the session was proving and the answer was not cached. */
  available: boolean;
  found: boolean;
  name: string;
  kind?: 'function' | 'macro' | 'theorem' | 'constant' | 'other';
  /**
   * Where it came from. Not derivable from `book`: ACL2's own names have no
   * book either, which is how `symbolp` once rendered as "defined in this
   * file".
   */
  origin?: 'file' | 'book' | 'builtin';
  formals?: string;
  guard?: string;
  statement?: string;
  /** As ACL2 names it: `(:system . "std/util/bstar.lisp")`, or a full path. */
  book?: string;
  /** `book` resolved to something openable. */
  path?: string;
  /** The topic summary, as raw xdoc markup. Render with `xdocInline`. */
  short?: string;
}

/**
 * The handful of XML entities xdoc uses that matter in a one-line summary.
 *
 * xdoc declares 141 in total, each with both a plain-text fallback and a
 * Unicode codepoint (`mdash` -> "---" / 8212, `Omega` -> "\\Omega" / 937).
 * The codepoint is the right choice for rendered output; the full table
 * belongs with the full renderer, and these are the ones that appear in
 * prose.
 */
export const ENTITIES: Record<string, string> = {
  amp: '&', lt: '<', gt: '>', quot: '"', apos: "'",
  nbsp: '\u00a0', ndash: '\u2013', mdash: '\u2014', hellip: '\u2026',
  lsquo: '\u2018', rsquo: '\u2019', ldquo: '\u201c', rdquo: '\u201d',
  larr: '\u2190', rarr: '\u2192', harr: '\u2194',
  ne: '\u2260', le: '\u2264', ge: '\u2265', times: '\u00d7',
};

/**
 * xdoc markup to markdown, for a *one-line* summary.
 *
 * The first slice of what the full topic renderer will need, and deliberately
 * only that: summaries are prose with cross-references, so what matters here
 * is `@(see …)`, `@('…')`, the inline tags, and entities. Block structure,
 * KaTeX and the world-reading directives (`@(def …)` and friends) belong with
 * the doc view, where there is room to resolve them.
 *
 * Markup arrives raw because the machinery that would expand it -- xdoc's own
 * `preprocess-main` -- is not in the world. Left alone, a
 * tooltip would read `@(see b*) binder for ...`.
 */
export function xdocInline(src: string): string {
  let t = src;
  // Cross-references first: their argument is a symbol, not markup.
  t = t.replace(/@\((?:see|csee|tsee|sym|csym|url|see\?)\s+([^)]*)\)/gi, (_m, a) =>
    '`' + String(a).trim() + '`'
  );
  // Inline and block code, before tag stripping so their contents are safe.
  t = t.replace(/@\('([^]*?)'\)/g, (_m, a) => '`' + String(a).trim() + '`');
  t = t.replace(/@\(\{([^]*?)\}\)/g, (_m, a) => '`' + String(a).trim().replace(/\s+/g, ' ') + '`');
  // Anything else in @(...) is a directive this slice does not resolve; show
  // the argument rather than the machinery.
  t = t.replace(/@\(\s*\S+\s+([^)]*)\)/g, (_m, a) => '`' + String(a).trim() + '`');
  // Inline tags. `<see topic="x">y</see>` keeps its text, not its target.
  t = t.replace(/<(?:v|tt|code)>([^]*?)<\/(?:v|tt|code)>/gi, (_m, a) => '`' + String(a).trim() + '`');
  t = t.replace(/<b>([^]*?)<\/b>/gi, '**$1**');
  t = t.replace(/<i>([^]*?)<\/i>/gi, '*$1*');
  t = t.replace(/<[^>]*>/g, '');
  t = t.replace(/&([A-Za-z]+);/g, (m, e) => ENTITIES[String(e)] ?? m);
  t = t.replace(/&#(\d+);/g, (_m, n) => String.fromCodePoint(Number(n)));
  return t.replace(/\s+/g, ' ').trim();
}

/** How a certification ended. */
export interface CertifyReply {
  status: 'certified' | 'failed';
  book: string;
  /** Books cert.pl blamed -- often a dependency, not the one asked for. */
  failed_books?: string[];
  error_line?: string;
  output_ref?: string;
  output_lines: number;
  elapsed_ms: number;
}

/** One topic's documentation, as raw xdoc markup. */
export interface DocReply {
  available: boolean;
  found: boolean;
  name: string;
  short?: string;
  long?: string;
}

/**
 * xdoc markup to readable plain text.
 *
 * The block-structure counterpart of `xdocInline`, for a whole topic. Where
 * that flattens everything to one line for a tooltip, this keeps paragraphs,
 * lists and -- above all -- code blocks, whose line structure carries meaning
 * and which the daemon transports verbatim for exactly that reason.
 *
 * Text rather than HTML for now: the full renderer, with `<see>` navigation
 * and KaTeX, wants a webview of its own. What this covers is the markup that
 * actually appears in bodies; the directives that read the world (`@(def …)`
 * and friends) are shown as the reference they are rather than resolved.
 */
export function xdocText(src: string): string {
  let t = src;
  // Code blocks first and verbatim: everything else rewrites inline, and a
  // code block's contents must not be touched by any of it.
  const blocks: string[] = [];
  t = t.replace(/@\(\{([^]*?)\}\)/g, (_m, code) => {
    blocks.push(String(code).replace(/^\n/, '').replace(/\s+$/, ''));
    return `\u0000CODE${blocks.length - 1}\u0000`;
  });
  t = t.replace(/<code>([^]*?)<\/code>/gi, (_m, code) => {
    blocks.push(String(code).replace(/^\n/, '').replace(/\s+$/, ''));
    return `\u0000CODE${blocks.length - 1}\u0000`;
  });
  // Math is shown as its source: rendering it needs KaTeX and a webview.
  t = t.replace(/@\(\[([^]*?)\]\)/g, (_m, a) => `\n    ${String(a).trim()}\n`);
  t = t.replace(/@\(\$([^]*?)\$\)/g, (_m, a) => String(a).trim());
  // Cross-references and inline code.
  t = t.replace(/@\((?:see|csee|tsee|sym|csym|url|see\?)\s+([^)]*)\)/gi, (_m, a) =>
    String(a).trim()
  );
  t = t.replace(/@\('([^]*?)'\)/g, (_m, a) => String(a).trim());
  // Anything left in @(...) reads the world; name it rather than resolve it.
  t = t.replace(/@\(\s*(\S+)\s+([^)]*)\)/g, (_m, d, a) =>
    `[${String(d).trim()} ${String(a).trim()}]`
  );
  // Block structure.
  t = t.replace(/<\/(?:p|blockquote|box|dd|dt)>/gi, '\n\n');
  t = t.replace(/<li>/gi, '\n  - ');
  t = t.replace(/<\/(?:ul|ol|dl|li)>/gi, '\n');
  t = t.replace(/<br\s*\/?>/gi, '\n');
  t = t.replace(/<h[1-5]>([^]*?)<\/h[1-5]>/gi, (_m, a) => `\n\n${String(a).trim()}\n`);
  t = t.replace(/<[^>]*>/g, '');
  t = t.replace(/&([A-Za-z]+);/g, (m, e) => ENTITIES[String(e)] ?? m);
  t = t.replace(/&#(\d+);/g, (_m, n) => String.fromCodePoint(Number(n)));
  // Collapse the whitespace the tags left behind, but never inside a block.
  t = t.replace(/[ \t]+/g, ' ').replace(/ ?\n ?/g, '\n').replace(/\n{3,}/g, '\n\n');
  t = t.replace(/\u0000CODE(\d+)\u0000/g, (_m, i) => `\n${blocks[Number(i)]}\n`);
  return t.trim();
}

/** One completion candidate from the daemon. */
export interface CompletionItemReply {
  name: string;
  kind: 'function' | 'macro' | 'theorem' | 'constant' | 'keyword' | 'other';
  /** The book it comes from, shown as the detail line. */
  book?: string;
}

export interface CompleteReply {
  items: CompletionItemReply[];
  /** The census was not ready or the list was capped; ask again as it narrows. */
  incomplete: boolean;
}

/**
 * The token being completed, ending at `character` on `lineText`.
 *
 * The same character class the daemon's `is_safe_symbol` accepts, and the
 * same one `language-configuration.json` calls a word -- so what the client
 * sends as a prefix is what the daemon can look up. A leading `:` is part of
 * the token, because that is what tells the daemon to offer keywords.
 */
export function prefixAt(lineText: string, character: number): string {
  const upto = lineText.slice(0, character);
  const m = /[^\s()[\]{}'"`,;|]*$/.exec(upto);
  return m ? m[0] : '';
}

/** One outline entry. Columns are 1-based and in bytes; see `positions.ts`. */
export interface BookSymbol {
  name: string;
  kind: 'function' | 'macro' | 'theorem' | 'constant' | 'include' | 'other';
  /** Present for top-level entries: the unit, so status can be attached. */
  unit?: number;
  line_start: number;
  line_end: number;
  col_start: number;
  col_end: number;
  children?: BookSymbol[];
}

/** One `include-book`, resolved. `path` absent means it did not resolve. */
export interface BookLink {
  line: number;
  /** 1-based byte column of the `(`, which separates two on one line. */
  column: number;
  book: string;
  path?: string;
}

/**
 * Where the book name sits on its line, as UTF-16 offsets.
 *
 * The daemon says which line, which column, and which name; the client
 * locates the quoted string itself, because searching one line in TypeScript
 * is UTF-16 native and the daemon's own offsets are bytes. `from` is where
 * that include's form starts, so two includes on one line find their own
 * names rather than both finding the first.
 *
 * The quotes are excluded from the range, so the link underlines the path
 * rather than the string literal.
 */
export function linkRangeOnLine(
  lineText: string,
  book: string,
  from = 0
): [number, number] | undefined {
  const at = lineText.indexOf(`"${book}"`, from);
  return at < 0 ? undefined : [at + 1, at + 1 + book.length];
}

/** Where a name is defined. `unit` means this file; `path` means another. */
export interface DefinitionReply {
  available: boolean;
  found: boolean;
  unit?: number;
  path?: string;
  /** Absent means "somewhere in that file" -- open it at the top. */
  line?: number;
  /** 1-based byte column on that line. Set exactly when `line` is. */
  column?: number;
  /** `definer` when the name is written at that line. */
  how?: string;
}

/**
 * ACL2's own name for a book, as a human would say it.
 *
 * Two shapes, the same two the dependency work resolves: a bare absolute
 * path, and `(:system . "rel/path.lisp")` for anything reached by
 * `:dir :system`. The system form is shown relative, because that is how it
 * is written in the `include-book` that pulled it in -- an absolute path
 * under someone's checkout is longer and says less.
 */
export function bookLabel(book: string | undefined): string | undefined {
  if (!book) {
    return undefined;
  }
  const quoted = /"([^"]+)"/.exec(book);
  if (quoted) {
    return quoted[1];
  }
  const t = book.trim();
  return t.length > 0 ? t : undefined;
}

/** `(alpha x)` from the name and ACL2's `(x)`. */
function callForm(name: string, formals: string): string {
  const inner = formals.trim().replace(/^\(/, '').replace(/\)$/, '').trim();
  return inner.length > 0 ? `(${name} ${inner})` : `(${name})`;
}

function fence(code: string): string {
  return '```lisp\n' + code + '\n```';
}

/**
 * The hover body, as markdown -- or `undefined` when there is nothing worth
 * showing.
 *
 * `available: false` renders as nothing rather than as an apology. It means
 * the prover was busy and the client should ask again, which it will do by
 * itself the next time the cursor moves; a tooltip saying "busy" would be a
 * worse answer than no tooltip, because it occupies the space where the real
 * answer goes and invites a re-read rather than a re-hover.
 */
export function hoverMarkdown(d: DescribeReply): string | undefined {
  if (!d.available || !d.found) {
    return undefined;
  }
  const parts: string[] = [];
  parts.push(`**${d.name}** *${d.kind ?? 'name'}*`);

  if (d.kind === 'theorem' && d.statement) {
    parts.push(fence(d.statement));
  } else if (d.formals) {
    parts.push(fence(callForm(d.name, d.formals)));
  }

  // A guard of `t` is every function's default and says nothing; ACL2 prints
  // the trivial one as the symbol, so it is dropped rather than shown as noise.
  if (d.guard && d.guard.trim().toLowerCase() !== 't') {
    parts.push(`Guard\n\n${fence(d.guard)}`);
  }

  // The summary sits above the provenance: it says what the thing *is*,
  // which is what someone hovering wants first.
  if (d.short) {
    const line = xdocInline(d.short);
    if (line) {
      parts.push(line);
    }
  }
  parts.push(provenance(d));
  return parts.join('\n\n');
}

/**
 * The "where did this come from" line.
 *
 * Driven by `origin`, never by the presence of a book. ACL2's own definitions
 * belong to no `include-book` and neither do this file's, so reading "no book"
 * as "this file" claimed that `symbolp` was defined in the buffer -- while
 * go-to-definition, which knew better, found nothing. The two now agree.
 */
function provenance(d: DescribeReply): string {
  const where = bookLabel(d.book);
  switch (d.origin) {
    case 'book':
      return where ? `Defined in \`${where}\`` : '*Defined in an included book*';
    case 'builtin':
      return '*Built into ACL2*';
    case 'file':
      return '*Defined in this file*';
    default:
      // An older daemon that does not send `origin`. Say only what is certain.
      return where ? `Defined in \`${where}\`` : '';
  }
}
