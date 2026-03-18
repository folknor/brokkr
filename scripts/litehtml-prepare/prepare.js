#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const https = require('https');
const http = require('http');
const { Buffer } = require('buffer');

const cheerio = require('cheerio');
const sizeOf = require('image-size');
const { PNG } = require('pngjs');

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const args = argv.slice(2);
  const command = args[0];

  if (command === 'prepare') {
    const input = args[1];
    const output = args[2];
    const opts = {};
    for (let i = 3; i < args.length; i += 2) {
      const key = args[i];
      const val = args[i + 1];
      if (key === '--cache-dir') opts.cacheDir = val;
      else if (key === '--ahem-font') opts.ahemFont = val;
      else if (key === '--fallback-aspect-ratio') opts.fallbackAspectRatio = parseFloat(val);
    }
    return { command, input, output, opts };
  }

  if (command === 'extract') {
    const input = args[1];
    const output = args[2];
    const opts = {};
    for (let i = 3; i < args.length; i += 2) {
      const key = args[i];
      const val = args[i + 1];
      if (key === '--selector') opts.selector = val;
    }
    return { command, input, output, opts };
  }

  process.stderr.write(`Usage:\n  node prepare.js prepare <input> <output> [options]\n  node prepare.js extract <input> <output> --selector <sel>\n`);
  process.exit(1);
}

// ---------------------------------------------------------------------------
// Warnings
// ---------------------------------------------------------------------------

function warn(msg) {
  process.stderr.write(`warning: ${msg}\n`);
}

// ---------------------------------------------------------------------------
// HTTP fetching
// ---------------------------------------------------------------------------

function fetchUrl(url, redirectCount) {
  if (redirectCount === undefined) redirectCount = 0;
  if (redirectCount > 5) return Promise.reject(new Error(`too many redirects: ${url}`));

  return new Promise((resolve, reject) => {
    const mod = url.startsWith('https') ? https : http;
    const req = mod.get(url, { timeout: 10000 }, (res) => {
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        let location = res.headers.location;
        if (location.startsWith('/')) {
          const parsed = new URL(url);
          location = parsed.origin + location;
        }
        resolve(fetchUrl(location, redirectCount + 1));
        return;
      }
      if (res.statusCode !== 200) {
        res.resume();
        reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        return;
      }
      const chunks = [];
      res.on('data', (chunk) => chunks.push(chunk));
      res.on('end', () => resolve(Buffer.concat(chunks)));
      res.on('error', reject);
    });
    req.on('error', reject);
    req.on('timeout', () => { req.destroy(); reject(new Error(`timeout fetching ${url}`)); });
  });
}

// ---------------------------------------------------------------------------
// Image cache
// ---------------------------------------------------------------------------

function cacheKey(url) {
  return crypto.createHash('sha256').update(url).digest('hex');
}

function cachedFetch(url, cacheDir) {
  const key = cacheKey(url);
  const cachePath = path.join(cacheDir, key);

  if (fs.existsSync(cachePath)) {
    return Promise.resolve(fs.readFileSync(cachePath));
  }

  return fetchUrl(url).then((buf) => {
    fs.writeFileSync(cachePath, buf);
    return buf;
  });
}

// ---------------------------------------------------------------------------
// Gray PNG generation
// ---------------------------------------------------------------------------

function grayPngDataUri(width, height) {
  const png = new PNG({ width, height });
  const gray = 0xd0;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const idx = (width * y + x) << 2;
      png.data[idx] = gray;
      png.data[idx + 1] = gray;
      png.data[idx + 2] = gray;
      png.data[idx + 3] = 0xff;
    }
  }
  const buf = PNG.sync.write(png, { colorType: 2 });
  return `data:image/png;base64,${buf.toString('base64')}`;
}

// ---------------------------------------------------------------------------
// Dimension resolution helpers
// ---------------------------------------------------------------------------

function parsePxValue(val) {
  if (!val) return null;
  val = String(val).trim();
  if (/^\d+(\.\d+)?$/.test(val)) return Math.round(parseFloat(val));
  const m = val.match(/^(\d+(?:\.\d+)?)px$/i);
  return m ? Math.round(parseFloat(m[1])) : null;
}

function dimensionsFromAttributes(el) {
  const w = parsePxValue(el.attr('width'));
  const h = parsePxValue(el.attr('height'));
  return (w && h) ? { width: w, height: h } : null;
}

function dimensionsFromInlineStyle(el) {
  const style = el.attr('style');
  if (!style) return null;
  const wm = style.match(/(?:^|;)\s*width\s*:\s*(\d+(?:\.\d+)?px)/i);
  const hm = style.match(/(?:^|;)\s*height\s*:\s*(\d+(?:\.\d+)?px)/i);
  if (wm && hm) {
    return { width: Math.round(parseFloat(wm[1])), height: Math.round(parseFloat(hm[1])) };
  }
  return null;
}

function singleDimensionFromAttrsOrStyle(el) {
  const w = parsePxValue(el.attr('width'));
  const h = parsePxValue(el.attr('height'));
  if (w) return { known: 'width', value: w };
  if (h) return { known: 'height', value: h };

  const style = el.attr('style');
  if (!style) return null;
  const wm = style.match(/(?:^|;)\s*width\s*:\s*(\d+(?:\.\d+)?px)/i);
  if (wm) return { known: 'width', value: Math.round(parseFloat(wm[1])) };
  const hm = style.match(/(?:^|;)\s*height\s*:\s*(\d+(?:\.\d+)?px)/i);
  if (hm) return { known: 'height', value: Math.round(parseFloat(hm[1])) };
  return null;
}

function isTrackingPixel(dims) {
  return dims && dims.width <= 3 && dims.height <= 3;
}

function fallbackDimensions(el, fallbackRatio) {
  // Size precedence for unfetchable images.
  const fromAttrs = dimensionsFromAttributes(el);
  if (fromAttrs) return fromAttrs;

  const fromStyle = dimensionsFromInlineStyle(el);
  if (fromStyle) return fromStyle;

  const single = singleDimensionFromAttrsOrStyle(el);
  if (single) {
    if (single.known === 'width') {
      return { width: single.value, height: Math.round(single.value / fallbackRatio) };
    }
    return { width: Math.round(single.value * fallbackRatio), height: single.value };
  }

  return null;
}

// ---------------------------------------------------------------------------
// SVG dimension resolution
// ---------------------------------------------------------------------------

function svgDimensions(el) {
  const w = parsePxValue(el.attr('width'));
  const h = parsePxValue(el.attr('height'));
  if (w && h) return { width: w, height: h };

  const viewBox = el.attr('viewBox') || el.attr('viewbox');
  if (viewBox) {
    const parts = viewBox.trim().split(/[\s,]+/);
    if (parts.length === 4) {
      const vw = parseFloat(parts[2]);
      const vh = parseFloat(parts[3]);
      if (vw > 0 && vh > 0) return { width: Math.round(vw), height: Math.round(vh) };
    }
  }

  const fromStyle = dimensionsFromInlineStyle(el);
  if (fromStyle) return fromStyle;

  return null;
}

// ---------------------------------------------------------------------------
// CSS background-image replacement
// ---------------------------------------------------------------------------

function stripBackgroundImages($) {
  // Inline styles
  $('[style]').each(function () {
    const el = $(this);
    const style = el.attr('style');
    if (!style) return;
    const replaced = style.replace(/background-image\s*:\s*url\([^)]+\)/gi, (match) => {
      // Only replace external URLs, not data URIs.
      if (/url\(\s*['"]?data:/i.test(match)) return match;
      warn(`stripped inline background-image: ${match.slice(0, 80)}`);
      return 'background-image: none';
    });
    if (replaced !== style) el.attr('style', replaced);
  });

  // <style> blocks
  $('style').each(function () {
    const el = $(this);
    const css = el.html();
    if (!css) return;
    const replaced = css.replace(/background-image\s*:\s*url\(\s*(['"]?)(?!data:)([^)'"]+)\1\s*\)/gi, (match) => {
      warn(`stripped CSS background-image: ${match.slice(0, 80)}`);
      return 'background-image: none';
    });
    if (replaced !== css) el.html(replaced);
  });
}

// ---------------------------------------------------------------------------
// External @import stripping
// ---------------------------------------------------------------------------

function stripExternalImports($) {
  $('style').each(function () {
    const el = $(this);
    const css = el.html();
    if (!css) return;
    const replaced = css.replace(/@import\s+url\(\s*(['"]?)([^)'"]+)\1\s*\)\s*;?/gi, (match, _q, url) => {
      if (url.startsWith('data:')) return match;
      warn(`stripped @import: ${url}`);
      return '';
    });
    if (replaced !== css) el.html(replaced);
  });
}

// ---------------------------------------------------------------------------
// img { background-color } hack removal
// ---------------------------------------------------------------------------

function stripImgBackgroundColorHack($) {
  $('style').each(function () {
    const el = $(this);
    const css = el.html();
    if (!css) return;
    // Remove rules like: img { background-color: #d0d0d0; }
    // This is a best-effort regex for the specific hack pattern.
    const replaced = css.replace(/img\s*\{[^}]*background-color\s*:[^;}]+;?\s*\}/gi, (match) => {
      // Only strip if the rule body is just background-color (possibly with other whitespace).
      const body = match.replace(/^img\s*\{/, '').replace(/\}$/, '').trim();
      if (/^background-color\s*:[^;]+;?\s*$/.test(body)) {
        return '';
      }
      // If there are other properties, just strip the background-color declaration.
      return match.replace(/background-color\s*:[^;}]+;?\s*/g, '');
    });
    if (replaced !== css) el.html(replaced);
  });
}

// ---------------------------------------------------------------------------
// Ahem font injection
// ---------------------------------------------------------------------------

function injectAhemFont($, ahemFontPath) {
  const fontData = fs.readFileSync(ahemFontPath);
  const b64 = fontData.toString('base64');
  const ext = path.extname(ahemFontPath).toLowerCase();
  const format = ext === '.woff2' ? 'woff2' : ext === '.woff' ? 'woff' : 'truetype';
  const mime = ext === '.woff2' ? 'font/woff2' : ext === '.woff' ? 'font/woff' : 'font/ttf';

  const styleContent = `
@font-face {
  font-family: 'ahem';
  src: url('data:${mime};base64,${b64}') format('${format}');
  font-weight: normal;
  font-style: normal;
}
* { font-family: 'ahem' !important; }
`;

  // Insert as the first <style> block in <head>.
  const head = $('head');
  if (head.length) {
    head.prepend(`<style>${styleContent}</style>`);
  }
}

// ---------------------------------------------------------------------------
// <picture>/<source> unwrapping
// ---------------------------------------------------------------------------

function unwrapPictureElements($) {
  $('picture').each(function () {
    const picture = $(this);
    const img = picture.find('img').first();
    if (img.length) {
      picture.replaceWith(img);
    } else {
      picture.remove();
    }
  });
}

// ---------------------------------------------------------------------------
// HTML pretty-printer
// ---------------------------------------------------------------------------

const VOID_ELEMENTS = new Set([
  'area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input',
  'link', 'meta', 'param', 'source', 'track', 'wbr',
]);

const INLINE_ELEMENTS = new Set([
  'a', 'abbr', 'b', 'bdi', 'bdo', 'br', 'cite', 'code', 'data',
  'dfn', 'em', 'i', 'img', 'kbd', 'mark', 'q', 'rp', 'rt', 'ruby',
  's', 'samp', 'small', 'span', 'strong', 'sub', 'sup', 'time', 'u',
  'var', 'wbr',
]);

const RAW_CONTENT_ELEMENTS = new Set(['script', 'style']);

function prettyPrint($) {
  const root = $.root();
  const lines = [];
  serializeChildren(root, 0, lines, $);
  return lines.join('\n') + '\n';
}

function serializeChildren(parent, depth, lines, $) {
  const children = parent.contents();
  children.each(function () {
    serializeNode($(this), depth, lines, $);
  });
}

function serializeNode(node, depth, lines, $) {
  const type = node[0] && node[0].type;
  const indent = '  '.repeat(depth);

  if (type === 'directive') {
    // Doctype
    lines.push(`${indent}${node.toString().trim()}`);
    return;
  }

  if (type === 'comment') {
    const text = node[0].data || '';
    lines.push(`${indent}<!--${text}-->`);
    return;
  }

  if (type === 'text') {
    const text = node.text();
    const trimmed = text.trim();
    if (!trimmed) return;
    // Short text on one line, longer text indented.
    lines.push(`${indent}${trimmed}`);
    return;
  }

  // htmlparser2 uses type 'script'/'style' for those elements, not 'tag'.
  if (type !== 'tag' && type !== 'script' && type !== 'style') return;

  const tagName = node[0].name;
  const attrs = serializeAttributes(node);
  const attrStr = attrs ? ' ' + attrs : '';

  if (VOID_ELEMENTS.has(tagName)) {
    lines.push(`${indent}<${tagName}${attrStr}>`);
    return;
  }

  if (RAW_CONTENT_ELEMENTS.has(tagName)) {
    const content = node.html();
    if (!content || !content.trim()) {
      lines.push(`${indent}<${tagName}${attrStr}></${tagName}>`);
      return;
    }
    lines.push(`${indent}<${tagName}${attrStr}>`);
    // Indent raw content lines.
    const contentLines = content.split('\n');
    for (const line of contentLines) {
      const trimmedLine = line.trimEnd();
      if (trimmedLine) {
        lines.push(`${indent}  ${trimmedLine.trimStart()}`);
      }
    }
    lines.push(`${indent}</${tagName}>`);
    return;
  }

  // Check if all children are inline/text (short content can go on one line).
  const children = node.contents();
  const childCount = children.length;
  if (childCount === 0) {
    lines.push(`${indent}<${tagName}${attrStr}></${tagName}>`);
    return;
  }

  const allInline = isAllInlineContent(children);
  if (allInline && node.text().length < 80) {
    lines.push(`${indent}<${tagName}${attrStr}>${node.html()}</${tagName}>`);
    return;
  }

  lines.push(`${indent}<${tagName}${attrStr}>`);
  serializeChildren(node, depth + 1, lines, $);
  lines.push(`${indent}</${tagName}>`);
}

function isAllInlineContent(children) {
  let allInline = true;
  children.each(function () {
    const type = this.type;
    if (type === 'text') return;
    if (type === 'tag' && INLINE_ELEMENTS.has(this.name)) return;
    allInline = false;
    return false; // break
  });
  return allInline;
}

function serializeAttributes(node) {
  const attribs = node[0].attribs;
  if (!attribs) return '';
  const parts = [];
  for (const [key, val] of Object.entries(attribs)) {
    if (val === '') {
      parts.push(key);
    } else {
      // Use double quotes; escape any double quotes in value.
      parts.push(`${key}="${val.replace(/"/g, '&quot;')}"`);
    }
  }
  return parts.join(' ');
}

// ---------------------------------------------------------------------------
// Prepare command
// ---------------------------------------------------------------------------

async function cmdPrepare(input, output, opts) {
  const html = fs.readFileSync(input, 'utf-8');
  const $ = cheerio.load(html, { decodeEntities: false });

  const cacheDir = opts.cacheDir;
  const ahemFont = opts.ahemFont;
  const fallbackRatio = opts.fallbackAspectRatio || 2.0;

  if (cacheDir) fs.mkdirSync(cacheDir, { recursive: true });

  // 1. Unwrap <picture>/<source> elements.
  unwrapPictureElements($);

  // 2. Process <img> elements.
  const imgs = $('img').toArray();
  for (const imgEl of imgs) {
    const el = $(imgEl);
    const src = el.attr('src');
    if (!src) continue;

    // Skip data URIs.
    if (src.startsWith('data:')) continue;

    // Skip non-HTTP URLs.
    if (!src.startsWith('http://') && !src.startsWith('https://')) continue;

    let dims = null;
    let fetched = false;

    // Try to fetch and read natural dimensions.
    if (cacheDir) {
      try {
        const buf = await cachedFetch(src, cacheDir);
        fetched = true;
        try {
          const size = sizeOf(buf);
          if (size.width && size.height) {
            dims = { width: size.width, height: size.height };
          }
        } catch (e) {
          warn(`could not read dimensions from ${src}: ${e.message}`);
        }
      } catch (e) {
        warn(`could not fetch ${src}: ${e.message}`);
      }
    }

    // Fallback dimension chain.
    if (!dims) {
      dims = fallbackDimensions(el, fallbackRatio);
      if (!dims) {
        warn(`no dimensions for ${src}, using 200x100`);
        dims = { width: 200, height: 100 };
      }
    }

    // Tracking pixel detection.
    if (isTrackingPixel(dims)) {
      dims = { width: 1, height: 1 };
    }

    // Replace src with gray PNG data URI.
    const dataUri = grayPngDataUri(dims.width, dims.height);
    el.attr('src', dataUri);

    // Set explicit width/height attributes.
    el.attr('width', String(dims.width));
    el.attr('height', String(dims.height));

    // Remove srcset if present.
    el.removeAttr('srcset');
  }

  // 3. Replace inline <svg> elements.
  $('svg').each(function () {
    const el = $(this);
    const dims = svgDimensions(el) || (() => {
      warn('inline SVG has no dimensions, using 100x100');
      return { width: 100, height: 100 };
    })();
    const dataUri = grayPngDataUri(dims.width, dims.height);
    const img = $(`<img src="${dataUri}" width="${dims.width}" height="${dims.height}">`);
    el.replaceWith(img);
  });

  // 4. Strip CSS background-images.
  stripBackgroundImages($);

  // 5. Strip external @import rules.
  stripExternalImports($);

  // 6. Strip img { background-color } CSS hacks.
  stripImgBackgroundColorHack($);

  // 7. Inject Ahem font.
  if (ahemFont) {
    injectAhemFont($, ahemFont);
  }

  // 8. Pretty-print and write output.
  const result = prettyPrint($);
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, result, 'utf-8');
}

// ---------------------------------------------------------------------------
// Extract command
// ---------------------------------------------------------------------------

function cmdExtract(input, output, opts) {
  const html = fs.readFileSync(input, 'utf-8');
  const $ = cheerio.load(html, { decodeEntities: false });

  const selector = opts.selector;
  if (!selector) {
    process.stderr.write('error: --selector is required\n');
    process.exit(1);
  }

  const matches = $(selector);
  if (matches.length === 0) {
    process.stderr.write(`error: selector '${selector}' matched no elements\n`);
    process.exit(1);
  }
  if (matches.length > 1) {
    const paths = [];
    matches.each(function () {
      const el = $(this);
      const tag = this.name;
      const id = el.attr('id');
      const cls = el.attr('class');
      let desc = tag;
      if (id) desc += `#${id}`;
      if (cls) desc += `.${cls.split(/\s+/).join('.')}`;
      paths.push(desc);
    });
    process.stderr.write(`error: selector '${selector}' matched ${matches.length} elements:\n`);
    for (const p of paths) {
      process.stderr.write(`  ${p}\n`);
    }
    process.exit(1);
  }

  const target = matches.first();

  // Collect head content (styles, meta).
  const headContent = [];
  $('head').children().each(function () {
    const el = $(this);
    const tag = this.name;
    if (tag === 'style' || tag === 'meta') {
      headContent.push($.html(el));
    }
  });

  // Walk ancestor chain from target up to body.
  const ancestors = [];
  let current = target;
  while (current.length && current[0].name !== 'body' && current[0].name !== 'html') {
    ancestors.unshift(current);
    current = current.parent();
  }

  // Build the extracted structure.
  const out = cheerio.load('<!DOCTYPE html><html><head></head><body></body></html>', { decodeEntities: false });

  // Copy head content.
  for (const h of headContent) {
    out('head').append(h);
  }

  // Reconstruct ancestor chain inside <body>.
  let parent = out('body');

  for (let i = 0; i < ancestors.length; i++) {
    const ancestorNode = ancestors[i];
    const isTarget = (i === ancestors.length - 1);
    const tagName = ancestorNode[0].name;
    const attrs = serializeAttributes(ancestorNode);
    const attrStr = attrs ? ' ' + attrs : '';

    if (isTarget) {
      // Clone the target with all its children.
      parent.append($.html(ancestorNode));
    } else {
      // Create the ancestor wrapper (tag + attributes, no other children).
      const wrapper = out(`<${tagName}${attrStr}></${tagName}>`);
      parent.append(wrapper);

      // Table context: preserve sibling <td>/<th> in the same <tr> as empty stubs.
      if (tagName === 'tr' || (ancestors[i + 1] && ancestors[i + 1][0].name === 'td') || (ancestors[i + 1] && ancestors[i + 1][0].name === 'th')) {
        // If this is a <tr>, stub out sibling cells.
        if (tagName === 'tr') {
          const nextAncestor = ancestors[i + 1];
          ancestorNode.children().each(function () {
            const child = $(this);
            const childTag = this.name;
            if ((childTag === 'td' || childTag === 'th') && !child.is(nextAncestor)) {
              const stubAttrs = serializeAttributes(child);
              const stubAttrStr = stubAttrs ? ' ' + stubAttrs : '';
              wrapper.append(out(`<${childTag}${stubAttrStr}></${childTag}>`));
            }
          });
        }
      }

      parent = wrapper;
    }
  }

  // If no ancestors (target was directly in body), just copy it.
  if (ancestors.length === 0) {
    parent.append($.html(target));
  }

  const result = prettyPrint(out);
  fs.mkdirSync(path.dirname(output), { recursive: true });
  fs.writeFileSync(output, result, 'utf-8');
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  const parsed = parseArgs(process.argv);

  if (parsed.command === 'prepare') {
    await cmdPrepare(parsed.input, parsed.output, parsed.opts);
  } else if (parsed.command === 'extract') {
    cmdExtract(parsed.input, parsed.output, parsed.opts);
  }
}

main().catch((err) => {
  process.stderr.write(`error: ${err.message}\n`);
  process.exit(1);
});
