/**
 * xdoc markup to HTML, for the documentation view.
 *
 * Free of `vscode`, like `names.ts` and `protocol.ts`, so `webview.mjs` can
 * check it under plain node -- which matters here more than anywhere else,
 * since this is the module with the most decisions per line, and the one
 * standing between a book's own markup and a script-enabled webview. What
 * `webview.mjs` checks is the *output*: the claim is that no book text
 * reaches the page as markup, and the way to check a claim about output is to
 * look at output. The page's script nonce is the second layer behind the same
 * threat; this module is the first.
 *
 * Markup arrives raw because the machinery that would expand it, xdoc's own
 * `preprocess-main`, is not in the world: only `xdoc/base.lisp`, which
 * defines the table, is universally pulled in.
 */

import { ENTITIES } from './names';

/** A code block lifted out of a body, with its text exactly as written. */
export interface DocCode {
  slot: number;
  text: string;
}

export interface DocHtml {
  html: string;
  /** Code blocks, for the host to colour with the editor's own grammar. */
  code: DocCode[];
}

const OPEN: Record<string, string> = {
  p: '<p>', blockquote: '<blockquote>', ul: '<ul>', ol: '<ol>', li: '<li>',
  dl: '<dl>', dt: '<dt>', dd: '<dd>', box: '<div class="box">',
  h1: '<h4>', h2: '<h4>', h3: '<h4>', h4: '<h5>', h5: '<h5>',
  table: '<table>', tr: '<tr>', th: '<th>', td: '<td>',
  b: '<b>', i: '<i>', u: '<u>', sf: '<span class="sf">',
  v: '<code>', tt: '<code>',
};
const CLOSE: Record<string, string> = {
  p: '</p>', blockquote: '</blockquote>', ul: '</ul>', ol: '</ol>', li: '</li>',
  dl: '</dl>', dt: '</dt>', dd: '</dd>', box: '</div>',
  h1: '</h4>', h2: '</h4>', h3: '</h4>', h4: '</h5>', h5: '</h5>',
  table: '</table>', tr: '</tr>', th: '</th>', td: '</td>',
  b: '</b>', i: '</i>', u: '</u>', sf: '</span>',
  v: '</code>', tt: '</code>',
};

const ESCAPES: Record<string, string> = {
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;',
};

/**
 * Exported because anyone assembling HTML *about* a topic needs it too: a
 * topic name reaches us from a book's own `<see topic="...">`, so it is
 * book-supplied text wherever it is put, not just inside rendered markup.
 */
export function esc(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ESCAPES[c]);
}

/** Entities to characters *before* escaping, so `&lt;` round-trips right. */
function ents(s: string): string {
  return s
    .replace(/&([A-Za-z]+);/g, (m, e) => ENTITIES[String(e)] ?? m)
    .replace(/&#(\d+);/g, (_m, n) => String.fromCodePoint(Number(n)));
}

/**
 * Drop the indentation a code block was written with, keeping its shape.
 *
 * xdoc bodies indent `<code>` blocks by a space on purpose -- ACL2's own
 * sources explain it stops Emacs reading the block's first line as a
 * top-level form -- so the indentation is a convention of the *source*, not
 * of the code. Removing the common prefix keeps every relative indent.
 */
function dedent(text: string): string {
  const lines = text.replace(/^\n/, '').replace(/\s+$/, '').split('\n');
  const indents = lines
    .filter((l) => l.trim().length > 0)
    .map((l) => (/^\s*/.exec(l) ?? [''])[0].length);
  const cut = indents.length > 0 ? Math.min(...indents) : 0;
  return lines.map((l) => l.slice(cut)).join('\n');
}

/**
 * A symbol as a reader writes it, from a symbol as ACL2 prints it.
 *
 * `|TREESET|::|ROTATE-LEFT$INLINE|` is one name. The bars are quoting, the
 * qualifier is the package, and neither is what someone reading a sentence
 * about the function wants to see -- every other name on this page is
 * `rotate-left`, so this one is too. Case is folded down because ACL2 prints
 * symbols upcased while the source that wrote them is lower.
 */
function symName(raw: string): string {
  const bare = raw.replace(/\|/g, '').trim();
  const last = bare.includes('::') ? bare.slice(bare.lastIndexOf('::') + 2) : bare;
  return last === last.toUpperCase() ? last.toLowerCase() : last;
}

function link(target: string, text: string): string {
  return `<a href="#" class="see" data-topic="${esc(target.trim())}">${esc(text.trim())}</a>`;
}

/**
 * Render one topic body.
 *
 * A scanner rather than a chain of regexes, because escaping and tag handling
 * have to happen in the same pass: escape first and the tags are gone, escape
 * last and a `<` written in prose has already been read as a tag.
 *
 * Code blocks are lifted out *before* anything else and returned separately,
 * so no later rewriting can reach inside them and so the host can colour them
 * with the editor's own grammar -- the same choice made for the form view's
 * terms, and for the same reason: a second, approximate Lisp lexer is
 * precisely what this project keeps declining to write.
 *
 * `@([…])` and `@($…$)` are shown as their TeX source. Rendering them needs
 * KaTeX, which is a separate decision about shipping a library into a
 * CSP-locked page; the source is honest and readable meanwhile. Directives
 * that read the world -- `@(def foo)` and friends -- are named rather than
 * resolved, for the same reason a hover declines rather than blocks.
 */
export function xdocHtml(src: string): DocHtml {
  const code: DocCode[] = [];
  const inline: string[] = [];
  // Sentinels are control characters, not letters. `I0` was indistinguishable
  // from prose that happens to contain "I0", and nothing in this pipeline
  // turns a U+0001 into anything else.
  const M = '\u0001';
  const holdInline = (html: string): string => {
    inline.push(html);
    return `${M}${inline.length - 1}${M}`;
  };
  /** A code block: lifted out whole, dedented, coloured by the host. */
  const holdBlock = (text: string): string => {
    const slot = code.length;
    // Entities decoded here, as they are for inline code below: a block is
    // lifted out before the pass that would otherwise do it, so left alone it
    // keeps `&rarr;` as five characters. Every `define` writes one -- its
    // generated signature line is `(f x) &rarr; y` -- so this is the common
    // case, not a corner of the markup.
    const body = dedent(ents(text));
    code.push({ slot, text: body });
    return holdInline(`<pre data-code="${slot}">${esc(body)}</pre>`);
  };
  /**
   * Inline code, which is a slot *too*, not a plain span.
   *
   * Most of a real topic's code is inline -- `@('foo')` runs all through the
   * prose -- so colouring only the blocks leaves nearly all of it grey, which
   * is exactly what the first version did.
   */
  const holdCode = (text: string): string => {
    const slot = code.length;
    code.push({ slot, text });
    return holdInline(`<code data-code="${slot}">${esc(text)}</code>`);
  };

  let t = src;
  t = t.replace(/@\(\{([^]*?)\}\)/g, (_m, c) => holdBlock(String(c)));
  t = t.replace(/<code>([^]*?)<\/code>/gi, (_m, c) => holdBlock(String(c)));
  t = t.replace(/@\('([^]*?)'\)/g, (_m, c) => holdCode(ents(String(c).trim())));
  t = t.replace(/<v>([^]*?)<\/v>/gi, (_m, c) => holdCode(ents(String(c).trim())));
  t = t.replace(/@\(\[([^]*?)\]\)/g, (_m, c) =>
    holdInline(`<div class="math">${esc(String(c).trim())}</div>`)
  );
  t = t.replace(/@\(\$([^]*?)\$\)/g, (_m, c) =>
    holdInline(`<span class="math">${esc(String(c).trim())}</span>`)
  );
  t = t.replace(/@\((?:see|csee|tsee|sym|csym|see\?)\s+([^)]*)\)/gi, (_m, a) =>
    holdInline(link(String(a), String(a)))
  );
  t = t.replace(/@\(url\s+([^)]*)\)/gi, (_m, a) => holdInline(esc(String(a).trim())));
  // The world-reading directives -- `@(def foo)`, `@(def? foo)`, `@(gdef
  // foo)`, `@(thm foo)`, `@(body foo)` and friends -- take a name and print
  // what the world holds under it. We do not print it; naming it is what we
  // can honestly do without a query per directive. The name itself is the
  // useful half, so it is shown as a name rather than as the directive with
  // its argument: ACL2 writes these package-qualified and vertical-bar
  // quoted, and `[def? |TREESET|::|ROTATE-LEFT$INLINE|]` is not something to
  // put in front of a reader.
  t = t.replace(/@\(\s*(def\??|gdef|thm|body|formals|measure|srclink)\s+([^)]*)\)/gi,
    (_m, _d, a) => holdInline(`<code class="xname">${esc(symName(String(a)))}</code>`)
  );
  t = t.replace(/@\(\s*(\S+)\s+([^)]*)\)/g, (_m, d, a) =>
    holdInline(
      `<span class="unresolved">[${esc(`${String(d).trim()} ${String(a).trim()}`)}]</span>`
    )
  );

  const out: string[] = [];
  const re = /<(\/?)([a-zA-Z][a-zA-Z0-9]*)((?:\s[^>]*)?)>/g;
  let at = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(t)) !== null) {
    out.push(esc(ents(t.slice(at, m.index))));
    at = m.index + m[0].length;
    const closing = m[1] === '/';
    const name = m[2].toLowerCase();
    if (name === 'see' || name === 'srclink') {
      if (closing) {
        out.push('</a>');
      } else {
        const topic = /topic\s*=\s*"([^"]*)"/i.exec(m[3] ?? '');
        out.push(`<a href="#" class="see" data-topic="${esc(topic ? topic[1] : '')}">`);
      }
      continue;
    }
    if (name === 'br') {
      out.push('<br>');
      continue;
    }
    // An unknown tag is dropped and its text kept: xdoc's vocabulary grows,
    // and losing the prose would be worse than losing the emphasis.
    const map = closing ? CLOSE : OPEN;
    if (map[name]) {
      out.push(map[name]);
    }
  }
  out.push(esc(ents(t.slice(at))));

  let html = out.join('');
  html = html.replace(/\u0001(\d+)\u0001/g, (_m, i) => inline[Number(i)]);
  return { html, code };
}
