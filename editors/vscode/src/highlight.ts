// Tokenizing the form view's terms with the very grammar the editor uses, and
// coloring them with the very theme rules the editor uses.
//
// A webview receives workbench colors but never the editor's token colors,
// so anything rendering code inside one has to answer both questions itself:
// what are the tokens, and what color is each. `vscode-textmate` answers
// both -- it is the engine VSCode tokenizes buffers with, and its registry
// resolves a theme's scope selectors with VSCode's own algorithm, so a term
// in the form view and the same term in the book agree by construction rather
// than by a palette chosen to look close.
//
// The theme itself is the one thing with no API. `workbench.colorTheme` is a
// label, so we find the extension contributing that label and read its JSON
// off disk. That can fail -- an unparseable file, a tmTheme plist, a theme
// that ships colors we cannot follow -- and when it does we fall back to the
// colors this extension contributes, which themes and users can set and
// which default to the terminal palette. Degraded, but never wrong-looking.

import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import * as oniguruma from 'vscode-oniguruma';
import * as vsctm from 'vscode-textmate';

/** One run of same-colored text. `c` is any CSS color; `i` marks italics. */
export interface Tok {
  t: string;
  c: string;
  i?: 1;
}

/** Tokens per line, so a multi-line term keeps its line structure. */
export type TokLines = Tok[][];

/** Foreground occupies bits 15-23 of a token's encoded metadata, style 11-14. */
const FG_MASK = 0b00000000111111111000000000000000;
const FG_SHIFT = 15;
const STYLE_MASK = 0b00000000000000000111100000000000;
const STYLE_SHIFT = 11;
const ITALIC = 1;

/**
 * Scope prefix to contributed color, longest prefix first. Only consulted
 * when the theme could not be read; the ids are the ones in package.json.
 */
const FALLBACK: Array<[string, string]> = [
  ['constant.other.keyword', 'keyword'],
  ['constant.numeric', 'number'],
  ['constant.character', 'string'],
  ['constant.language', 'constant'],
  ['variable.other.constant', 'constant'],
  ['entity.name', 'name'],
  ['storage', 'storage'],
  ['keyword', 'control'],
  ['comment', 'comment'],
  ['string', 'string'],
  ['punctuation', 'punctuation'],
];

function fallbackColor(scopes: string[]): string {
  for (let i = scopes.length - 1; i >= 0; i--) {
    for (const [prefix, id] of FALLBACK) {
      if (scopes[i].startsWith(prefix)) {
        return `var(--vscode-acl2Session-token-${id})`;
      }
    }
  }
  return '';
}

/**
 * Strip comments and trailing commas. Theme files are JSONC, and the parser
 * VSCode uses for them is not on an extension's surface.
 *
 * Exported for `webview.mjs`, which is the only way either of the two pure
 * functions in this module gets checked: everything else here needs a real
 * VSCode to have an opinion about.
 */
export function stripJsonc(src: string): string {
  let out = '';
  let inString = false;
  for (let i = 0; i < src.length; i++) {
    const c = src[i];
    const next = src[i + 1];
    if (inString) {
      out += c;
      if (c === '\\') {
        out += next ?? '';
        i++;
      } else if (c === '"') {
        inString = false;
      }
      continue;
    }
    if (c === '"') {
      inString = true;
      out += c;
    } else if (c === '/' && next === '/') {
      while (i < src.length && src[i] !== '\n') {
        i++;
      }
      out += '\n';
    } else if (c === ',') {
      // A trailing comma, decided here rather than by a pass over the result:
      // the loop knows whether it is inside a string and a regex over the
      // finished text does not, so `"a, }"` in a theme's own value had its
      // comma taken out. Every other step of this parser is string-aware; this
      // was the one that was not.
      let j = i + 1;
      while (j < src.length && /\s/.test(src[j])) {
        j++;
      }
      if (src[j] !== '}' && src[j] !== ']') {
        out += c;
      }
    } else if (c === '/' && next === '*') {
      i += 2;
      while (i < src.length && !(src[i] === '*' && src[i + 1] === '/')) {
        i++;
      }
      i++;
    } else {
      out += c;
    }
  }
  return out;
}

/** The file behind the active `workbench.colorTheme` label, if we can name it. */
function activeThemeFile(): string | undefined {
  const label = vscode.workspace.getConfiguration('workbench').get<string>('colorTheme');
  if (!label) {
    return undefined;
  }
  for (const ext of vscode.extensions.all) {
    const themes = ext.packageJSON?.contributes?.themes as
      | Array<{ id?: string; label?: string; path?: string }>
      | undefined;
    for (const t of themes ?? []) {
      if ((t.id === label || t.label === label) && t.path) {
        return path.join(ext.extensionPath, t.path);
      }
    }
  }
  return undefined;
}

interface ThemeJson {
  include?: string;
  tokenColors?: unknown;
  /** The workbench colours: `editorError.foreground` and friends. */
  colors?: Record<string, string>;
}

/** Read a theme and everything it includes, base rules first so later ones win. */
function readTheme(file: string, depth = 0): ThemeJson | undefined {
  let obj: ThemeJson;
  try {
    obj = JSON.parse(stripJsonc(fs.readFileSync(file, 'utf8'))) as ThemeJson;
  } catch {
    return undefined;
  }
  if (!obj.include || depth >= 8) {
    return obj;
  }
  const base = readTheme(path.join(path.dirname(file), obj.include), depth + 1);
  if (!base) {
    return obj;
  }
  const rules = [
    ...(Array.isArray(base.tokenColors) ? base.tokenColors : []),
    ...(Array.isArray(obj.tokenColors) ? obj.tokenColors : []),
  ];
  // `colors` merges key by key, like the rules concatenate: a theme that
  // includes another and restates two colours means the other's rest still
  // apply. A plain spread would drop them.
  const colors = { ...(base.colors ?? {}), ...(obj.colors ?? {}) };
  return { ...base, ...obj, tokenColors: rules, colors };
}

/** The shape vscode-textmate wants; its own interface is not exported. */
export interface ThemeSetting {
  name?: string;
  scope?: string | string[];
  settings: { foreground?: string; fontStyle?: string; background?: string };
}

function asRules(v: unknown): ThemeSetting[] {
  return Array.isArray(v) ? (v as ThemeSetting[]) : [];
}

/**
 * The scopes VSCode's own simple `tokenColorCustomizations` keys stand for.
 *
 * `"editor.tokenColorCustomizations": { "comments": "#f00" }` is the spelling
 * the settings UI offers and the documentation leads with, and it reached
 * nothing here: only `textMateRules` was read, so a reader who set it saw red
 * comments in the buffer and theme-coloured ones in the form view -- from the
 * same grammar, which is the one thing the form view promises.
 *
 * The scopes are VSCode's documented set for these keys. Kept deliberately
 * broad rather than exact: a scope that is too general colours a token the
 * editor also colours, while one that is missing colours nothing, and of the
 * two failures the second is the one a reader notices as a discrepancy.
 */
const SIMPLE_SCOPES: Record<string, string[]> = {
  comments: ['comment'],
  strings: ['string'],
  keywords: ['keyword', 'storage', 'storage.type'],
  numbers: ['constant.numeric'],
  types: ['entity.name.type', 'entity.name.class', 'support.type', 'support.class'],
  functions: ['entity.name.function', 'support.function'],
  variables: ['variable', 'entity.name.variable'],
};

/**
 * The simple keys of one `tokenColorCustomizations` object, as TextMate rules.
 *
 * A value is either a colour or `{ foreground, fontStyle }`, which is what a
 * `ThemeSetting` already carries -- so this is a translation of the *key*, not
 * of the value.
 */
export function simpleRules(custom: Record<string, unknown>): ThemeSetting[] {
  const out: ThemeSetting[] = [];
  for (const [key, scopes] of Object.entries(SIMPLE_SCOPES)) {
    const v = custom[key];
    if (v === undefined || v === null) {
      continue;
    }
    const settings =
      typeof v === 'string'
        ? { foreground: v }
        : (v as { foreground?: string; fontStyle?: string });
    if (!settings.foreground && !settings.fontStyle) {
      continue;
    }
    out.push({ scope: scopes, settings });
  }
  return out;
}

/**
 * The rules the editor is painting with: the theme's, then the user's
 * overrides -- global first, then any keyed to this theme by name, since
 * later rules win.
 */
function themeRules(): ThemeSetting[] | undefined {
  const file = activeThemeFile();
  if (!file) {
    return undefined;
  }
  const theme = readTheme(file);
  // A `tokenColors` that is a path rather than a list means a tmTheme plist,
  // which we do not parse; the contributed colors carry that case.
  if (!theme || !Array.isArray(theme.tokenColors)) {
    return undefined;
  }
  const label = vscode.workspace.getConfiguration('workbench').get<string>('colorTheme');
  const custom =
    vscode.workspace
      .getConfiguration('editor')
      .get<Record<string, unknown>>('tokenColorCustomizations') ?? {};
  const scoped = label
    ? (custom[`[${label}]`] as Record<string, unknown> | undefined)
    : undefined;
  // Simple keys before `textMateRules` at each level, matching VSCode: a rule
  // written out by scope is the more specific statement and later rules win.
  return [
    ...asRules(theme.tokenColors),
    ...simpleRules(custom),
    ...asRules(custom['textMateRules']),
    ...(scoped ? simpleRules(scoped) : []),
    ...asRules(scoped?.['textMateRules']),
  ];
}

export class Highlighter {
  private grammar?: vsctm.IGrammar;
  private colorMap: string[] = [];
  private themed = false;
  private wasmLoaded = false;

  constructor(private readonly extensionPath: string) {}

  /** Build (or rebuild, after a theme change) the registry and grammar. */
  async load(): Promise<void> {
    try {
      if (!this.wasmLoaded) {
        const wasm = path.join(path.dirname(require.resolve('vscode-oniguruma')), 'onig.wasm');
        const bytes = fs.readFileSync(wasm);
        await oniguruma.loadWASM(
          bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
        );
        this.wasmLoaded = true;
      }
      const rules = themeRules();
      const file = path.join(this.extensionPath, 'syntaxes', 'acl2.tmLanguage.json');
      const registry = new vsctm.Registry({
        onigLib: Promise.resolve({
          createOnigScanner: (s: string[]) => new oniguruma.OnigScanner(s),
          createOnigString: (s: string) => new oniguruma.OnigString(s),
        }),
        theme: rules ? { name: 'active', settings: rules } : undefined,
        loadGrammar: (scope: string) =>
          Promise.resolve(
            scope === 'source.acl2'
              ? vsctm.parseRawGrammar(fs.readFileSync(file, 'utf8'), file)
              : null
          ),
      });
      const grammar = await registry.loadGrammar('source.acl2');
      if (!grammar) {
        return;
      }
      this.grammar = grammar;
      this.colorMap = registry.getColorMap();
      this.themed = rules !== undefined;
    } catch {
      // Leave whatever loaded last in place; tokenize() degrades to plain text.
    }
  }

  get ready(): boolean {
    return this.grammar !== undefined;
  }

  /** Tokenize one term. Returns undefined if the grammar never loaded. */
  tokenize(text: string): TokLines | undefined {
    const grammar = this.grammar;
    if (!grammar) {
      return undefined;
    }
    const out: TokLines = [];
    let stack = vsctm.INITIAL;
    for (const line of text.split('\n')) {
      const toks: Tok[] = [];
      if (this.themed) {
        const r = grammar.tokenizeLine2(line, stack);
        stack = r.ruleStack;
        for (let i = 0; i < r.tokens.length; i += 2) {
          const start = r.tokens[i];
          const end = i + 2 < r.tokens.length ? r.tokens[i + 2] : line.length;
          const meta = r.tokens[i + 1];
          const style = (meta & STYLE_MASK) >>> STYLE_SHIFT;
          push(toks, line.substring(start, end), this.colorMap[(meta & FG_MASK) >>> FG_SHIFT], style);
        }
      } else {
        const r = grammar.tokenizeLine(line, stack);
        stack = r.ruleStack;
        for (const t of r.tokens) {
          push(toks, line.substring(t.startIndex, t.endIndex), fallbackColor(t.scopes), 0);
        }
      }
      out.push(toks);
    }
    return out;
  }
}

/** Append, merging into the previous run when the styling is identical. */
function push(toks: Tok[], text: string, color: string, style: number): void {
  if (!text) {
    return;
  }
  const italic = (style & ITALIC) === ITALIC ? 1 : undefined;
  const last = toks[toks.length - 1];
  if (last && last.c === color && last.i === italic) {
    last.t += text;
    return;
  }
  toks.push(italic ? { t: text, c: color, i: italic } : { t: text, c: color });
}

/**
 * The active theme's workbench colours, when the theme can be read at all.
 *
 * The same best-effort read the token colours get, and it fails the same
 * ways: a theme whose label does not match a contributed one, a `.tmTheme`
 * plist rather than JSON, or a theme that simply does not mention the colour
 * being asked for -- most define a fraction of them and let VSCode's built-in
 * defaults fill the rest, and those defaults live in no file. `undefined`
 * therefore means "could not read", not "the theme has no colours", and the
 * caller is expected to have something to fall back on.
 */
export function themeColors(): Record<string, string> | undefined {
  const file = activeThemeFile();
  if (!file) {
    return undefined;
  }
  return readTheme(file)?.colors;
}
