#!/usr/bin/env node
"use strict";

const fs = require("fs");
const path = require("path");
const crypto = require("crypto");
const https = require("https");
const http = require("http");
const { Buffer } = require("buffer");

const cheerio = require("cheerio");
const sizeOf = require("image-size");
const { PNG } = require("pngjs");

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

function parseArgs(argv) {
	const args = argv.slice(2);
	const command = args[0];

	if (command === "prepare") {
		const input = args[1];
		const output = args[2];
		const opts = {};
		for (let i = 3; i < args.length; i += 2) {
			const key = args[i];
			const val = args[i + 1];
			if (key === "--cache-dir") opts.cacheDir = val;
			else if (key === "--ahem-font") opts.ahemFont = val;
			else if (key === "--fallback-aspect-ratio")
				opts.fallbackAspectRatio = parseFloat(val);
		}
		return { command, input, output, opts };
	}

	if (command === "extract") {
		const input = args[1];
		const output = args[2];
		const opts = {};
		for (let i = 3; i < args.length; i += 2) {
			const key = args[i];
			const val = args[i + 1];
			if (key === "--selector") opts.selector = val;
			else if (key === "--from") opts.from = val;
			else if (key === "--to") opts.to = val;
		}
		return { command, input, output, opts };
	}

	if (command === "outline") {
		const input = args[1];
		const opts = { depth: 4, full: false, selectors: false };
		for (let i = 2; i < args.length; i++) {
			const key = args[i];
			if (key === "--depth") {
				opts.depth = parseInt(args[++i], 10);
			} else if (key === "--full") {
				opts.full = true;
			} else if (key === "--selectors") {
				opts.selectors = true;
			}
		}
		return { command, input, opts };
	}

	process.stderr.write(
		`Usage:\n  node prepare.js prepare <input> <output> [options]\n  node prepare.js extract <input> <output> --selector <sel>\n  node prepare.js outline <input> [--depth N] [--full] [--selectors]\n`,
	);
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
	if (redirectCount > 5)
		return Promise.reject(new Error(`too many redirects: ${url}`));

	return new Promise((resolve, reject) => {
		const mod = url.startsWith("https") ? https : http;
		const req = mod.get(url, { timeout: 10000 }, (res) => {
			if (
				res.statusCode >= 300 &&
				res.statusCode < 400 &&
				res.headers.location
			) {
				let location = res.headers.location;
				if (location.startsWith("/")) {
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
			res.on("data", (chunk) => chunks.push(chunk));
			res.on("end", () => resolve(Buffer.concat(chunks)));
			res.on("error", reject);
		});
		req.on("error", reject);
		req.on("timeout", () => {
			req.destroy();
			reject(new Error(`timeout fetching ${url}`));
		});
	});
}

// ---------------------------------------------------------------------------
// Image cache
// ---------------------------------------------------------------------------

function cacheKey(url) {
	return crypto.createHash("sha256").update(url).digest("hex");
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
	return `data:image/png;base64,${buf.toString("base64")}`;
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
	const w = parsePxValue(el.attr("width"));
	const h = parsePxValue(el.attr("height"));
	return w && h ? { width: w, height: h } : null;
}

function dimensionsFromInlineStyle(el) {
	const style = el.attr("style");
	if (!style) return null;
	const wm = style.match(/(?:^|;)\s*width\s*:\s*(\d+(?:\.\d+)?px)/i);
	const hm = style.match(/(?:^|;)\s*height\s*:\s*(\d+(?:\.\d+)?px)/i);
	if (wm && hm) {
		return {
			width: Math.round(parseFloat(wm[1])),
			height: Math.round(parseFloat(hm[1])),
		};
	}
	return null;
}

function singleDimensionFromAttrsOrStyle(el) {
	const w = parsePxValue(el.attr("width"));
	const h = parsePxValue(el.attr("height"));
	if (w) return { known: "width", value: w };
	if (h) return { known: "height", value: h };

	const style = el.attr("style");
	if (!style) return null;
	const wm = style.match(/(?:^|;)\s*width\s*:\s*(\d+(?:\.\d+)?px)/i);
	if (wm) return { known: "width", value: Math.round(parseFloat(wm[1])) };
	const hm = style.match(/(?:^|;)\s*height\s*:\s*(\d+(?:\.\d+)?px)/i);
	if (hm) return { known: "height", value: Math.round(parseFloat(hm[1])) };
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
		if (single.known === "width") {
			return {
				width: single.value,
				height: Math.round(single.value / fallbackRatio),
			};
		}
		return {
			width: Math.round(single.value * fallbackRatio),
			height: single.value,
		};
	}

	return null;
}

// ---------------------------------------------------------------------------
// SVG dimension resolution
// ---------------------------------------------------------------------------

function svgDimensions(el) {
	const w = parsePxValue(el.attr("width"));
	const h = parsePxValue(el.attr("height"));
	if (w && h) return { width: w, height: h };

	const viewBox = el.attr("viewBox") || el.attr("viewbox");
	if (viewBox) {
		const parts = viewBox.trim().split(/[\s,]+/);
		if (parts.length === 4) {
			const vw = parseFloat(parts[2]);
			const vh = parseFloat(parts[3]);
			if (vw > 0 && vh > 0)
				return { width: Math.round(vw), height: Math.round(vh) };
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
	$("[style]").each(function () {
		const el = $(this);
		const style = el.attr("style");
		if (!style) return;
		const replaced = style.replace(
			/background-image\s*:\s*url\([^)]+\)/gi,
			(match) => {
				// Only replace external URLs, not data URIs.
				if (/url\(\s*['"]?data:/i.test(match)) return match;
				warn(`stripped inline background-image: ${match.slice(0, 80)}`);
				return "background-image: none";
			},
		);
		if (replaced !== style) el.attr("style", replaced);
	});

	// <style> blocks
	$("style").each(function () {
		const el = $(this);
		const css = el.html();
		if (!css) return;
		const replaced = css.replace(
			/background-image\s*:\s*url\(\s*(['"]?)(?!data:)([^)'"]+)\1\s*\)/gi,
			(match) => {
				warn(`stripped CSS background-image: ${match.slice(0, 80)}`);
				return "background-image: none";
			},
		);
		if (replaced !== css) el.html(replaced);
	});
}

// ---------------------------------------------------------------------------
// External @import stripping
// ---------------------------------------------------------------------------

function stripExternalImports($) {
	$("style").each(function () {
		const el = $(this);
		const css = el.html();
		if (!css) return;
		const replaced = css.replace(
			/@import\s+url\(\s*(['"]?)([^)'"]+)\1\s*\)\s*;?/gi,
			(match, _q, url) => {
				if (url.startsWith("data:")) return match;
				warn(`stripped @import: ${url}`);
				return "";
			},
		);
		if (replaced !== css) el.html(replaced);
	});
}

// ---------------------------------------------------------------------------
// img { background-color } hack removal
// ---------------------------------------------------------------------------

function stripImgBackgroundColorHack($) {
	$("style").each(function () {
		const el = $(this);
		const css = el.html();
		if (!css) return;
		// Remove rules like: img { background-color: #d0d0d0; }
		// This is a best-effort regex for the specific hack pattern.
		const replaced = css.replace(
			/img\s*\{[^}]*background-color\s*:[^;}]+;?\s*\}/gi,
			(match) => {
				// Only strip if the rule body is just background-color (possibly with other whitespace).
				const body = match
					.replace(/^img\s*\{/, "")
					.replace(/\}$/, "")
					.trim();
				if (/^background-color\s*:[^;]+;?\s*$/.test(body)) {
					return "";
				}
				// If there are other properties, just strip the background-color declaration.
				return match.replace(/background-color\s*:[^;}]+;?\s*/g, "");
			},
		);
		if (replaced !== css) el.html(replaced);
	});
}

// ---------------------------------------------------------------------------
// Ahem font injection
// ---------------------------------------------------------------------------

function injectAhemFont($, ahemFontPath) {
	const fontData = fs.readFileSync(ahemFontPath);
	const b64 = fontData.toString("base64");
	const ext = path.extname(ahemFontPath).toLowerCase();
	const format =
		ext === ".woff2" ? "woff2" : ext === ".woff" ? "woff" : "truetype";
	const mime =
		ext === ".woff2"
			? "font/woff2"
			: ext === ".woff"
				? "font/woff"
				: "font/ttf";

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
	const head = $("head");
	if (head.length) {
		head.prepend(`<style>${styleContent}</style>`);
	}
}

// ---------------------------------------------------------------------------
// <picture>/<source> unwrapping
// ---------------------------------------------------------------------------

function unwrapPictureElements($) {
	$("picture").each(function () {
		const picture = $(this);
		const img = picture.find("img").first();
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
	"area",
	"base",
	"br",
	"col",
	"embed",
	"hr",
	"img",
	"input",
	"link",
	"meta",
	"param",
	"source",
	"track",
	"wbr",
]);

const INLINE_ELEMENTS = new Set([
	"a",
	"abbr",
	"b",
	"bdi",
	"bdo",
	"br",
	"cite",
	"code",
	"data",
	"dfn",
	"em",
	"i",
	"img",
	"kbd",
	"mark",
	"q",
	"rp",
	"rt",
	"ruby",
	"s",
	"samp",
	"small",
	"span",
	"strong",
	"sub",
	"sup",
	"time",
	"u",
	"var",
	"wbr",
]);

const RAW_CONTENT_ELEMENTS = new Set(["script", "style"]);

function prettyPrint($) {
	const root = $.root();
	const lines = [];
	serializeChildren(root, 0, lines, $);
	return lines.join("\n") + "\n";
}

function serializeChildren(parent, depth, lines, $) {
	const children = parent.contents();
	children.each(function () {
		serializeNode($(this), depth, lines, $);
	});
}

function serializeNode(node, depth, lines, $) {
	const type = node[0] && node[0].type;
	const indent = "  ".repeat(depth);

	if (type === "directive") {
		// Doctype
		lines.push(`${indent}${node.toString().trim()}`);
		return;
	}

	if (type === "comment") {
		const text = node[0].data || "";
		lines.push(`${indent}<!--${text}-->`);
		return;
	}

	if (type === "text") {
		const text = node.text();
		const trimmed = text.trim();
		if (!trimmed) return;
		// Short text on one line, longer text indented.
		lines.push(`${indent}${trimmed}`);
		return;
	}

	// htmlparser2 uses type 'script'/'style' for those elements, not 'tag'.
	if (type !== "tag" && type !== "script" && type !== "style") return;

	const tagName = node[0].name;
	const attrs = serializeAttributes(node);
	const attrStr = attrs ? " " + attrs : "";

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
		const contentLines = content.split("\n");
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
		if (type === "text") return;
		if (type === "tag" && INLINE_ELEMENTS.has(this.name)) return;
		allInline = false;
		return false; // break
	});
	return allInline;
}

function serializeAttributes(node) {
	const attribs = node[0].attribs;
	if (!attribs) return "";
	const parts = [];
	for (const [key, val] of Object.entries(attribs)) {
		if (val === "") {
			parts.push(key);
		} else {
			// Use double quotes; escape any double quotes in value.
			parts.push(`${key}="${val.replace(/"/g, "&quot;")}"`);
		}
	}
	return parts.join(" ");
}

// ---------------------------------------------------------------------------
// Prepare command
// ---------------------------------------------------------------------------

async function cmdPrepare(input, output, opts) {
	const html = fs.readFileSync(input, "utf-8");
	const $ = cheerio.load(html, { decodeEntities: false });

	const cacheDir = opts.cacheDir;
	const ahemFont = opts.ahemFont;
	const fallbackRatio = opts.fallbackAspectRatio || 2.0;

	if (cacheDir) fs.mkdirSync(cacheDir, { recursive: true });

	// 1. Unwrap <picture>/<source> elements.
	unwrapPictureElements($);

	// 2. Process <img> elements.
	const imgs = $("img").toArray();
	for (const imgEl of imgs) {
		const el = $(imgEl);
		const src = el.attr("src");
		if (!src) continue;

		// Skip data URIs.
		if (src.startsWith("data:")) continue;

		// Skip non-HTTP URLs.
		if (!src.startsWith("http://") && !src.startsWith("https://")) continue;

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
		el.attr("src", dataUri);

		// Set explicit width/height attributes.
		el.attr("width", String(dims.width));
		el.attr("height", String(dims.height));

		// Remove srcset if present.
		el.removeAttr("srcset");
	}

	// 3. Replace inline <svg> elements.
	$("svg").each(function () {
		const el = $(this);
		const dims =
			svgDimensions(el) ||
			(() => {
				warn("inline SVG has no dimensions, using 100x100");
				return { width: 100, height: 100 };
			})();
		const dataUri = grayPngDataUri(dims.width, dims.height);
		const img = $(
			`<img src="${dataUri}" width="${dims.width}" height="${dims.height}">`,
		);
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
	fs.writeFileSync(output, result, "utf-8");
}

// ---------------------------------------------------------------------------
// Extract command
// ---------------------------------------------------------------------------

function resolveOne($, selector, label) {
	const matches = $(selector);
	if (matches.length === 0) {
		process.stderr.write(
			`error: ${label} selector '${selector}' matched no elements\n`,
		);
		process.exit(1);
	}
	if (matches.length > 1) {
		const paths = [];
		matches.each(function () {
			const el = $(this);
			const tag = this.name;
			const id = el.attr("id");
			const cls = el.attr("class");
			let desc = tag;
			if (id) desc += `#${id}`;
			if (cls) desc += `.${cls.split(/\s+/).join(".")}`;
			paths.push(desc);
		});
		process.stderr.write(
			`error: ${label} selector '${selector}' matched ${matches.length} elements:\n`,
		);
		for (const p of paths) {
			process.stderr.write(`  ${p}\n`);
		}
		process.exit(1);
	}
	return matches.first();
}

function collectHeadContent($) {
	const headContent = [];
	$("head")
		.children()
		.each(function () {
			const el = $(this);
			const tag = this.name;
			if (tag === "style" || tag === "meta") {
				headContent.push($.html(el));
			}
		});
	return headContent;
}

function buildAncestorChain(el) {
	const ancestors = [];
	let current = el;
	while (
		current.length &&
		current[0].name !== "body" &&
		current[0].name !== "html"
	) {
		ancestors.unshift(current);
		current = current.parent();
	}
	return ancestors;
}

function buildExtractedDoc($, headContent, ancestors, targetHtmlFragments) {
	const out = cheerio.load(
		"<!DOCTYPE html><html><head></head><body></body></html>",
		{ decodeEntities: false },
	);

	for (const h of headContent) {
		out("head").append(h);
	}

	let parent = out("body");

	for (let i = 0; i < ancestors.length; i++) {
		const ancestorNode = ancestors[i];
		const isTarget = i === ancestors.length - 1;
		const tagName = ancestorNode[0].name;
		const attrs = serializeAttributes(ancestorNode);
		const attrStr = attrs ? " " + attrs : "";

		if (isTarget) {
			// At the target level, append all target fragments.
			for (const frag of targetHtmlFragments) {
				parent.append(frag);
			}
		} else {
			const wrapper = out(`<${tagName}${attrStr}></${tagName}>`);
			parent.append(wrapper);

			// Table context: preserve sibling <td>/<th> in the same <tr> as empty stubs.
			if (tagName === "tr") {
				const nextAncestor = ancestors[i + 1];
				ancestorNode.children().each(function () {
					const child = $(this);
					const childTag = this.name;
					if (
						(childTag === "td" || childTag === "th") &&
						!child.is(nextAncestor)
					) {
						const stubAttrs = serializeAttributes(child);
						const stubAttrStr = stubAttrs ? " " + stubAttrs : "";
						wrapper.append(out(`<${childTag}${stubAttrStr}></${childTag}>`));
					}
				});
			}

			parent = wrapper;
		}
	}

	if (ancestors.length === 0) {
		for (const frag of targetHtmlFragments) {
			parent.append(frag);
		}
	}

	return prettyPrint(out);
}

function cmdExtract(input, output, opts) {
	const html = fs.readFileSync(input, "utf-8");
	const $ = cheerio.load(html, { decodeEntities: false });

	const headContent = collectHeadContent($);

	if (opts.from && opts.to) {
		// Range extraction: --from / --to
		const fromEl = resolveOne($, opts.from, "--from");
		const toEl = resolveOne($, opts.to, "--to");

		// Find the closest common ancestor.
		// Build ancestor sets for both elements, then find where they diverge.
		function ancestorPath(el) {
			const path = [];
			let cur = el;
			while (cur.length && cur[0].name !== "html") {
				path.unshift(cur[0]);
				cur = cur.parent();
			}
			return path;
		}

		const fromPath = ancestorPath(fromEl);
		const toPath = ancestorPath(toEl);

		// Walk both paths to find the deepest common node.
		let commonDepth = 0;
		while (
			commonDepth < fromPath.length &&
			commonDepth < toPath.length &&
			fromPath[commonDepth] === toPath[commonDepth]
		) {
			commonDepth++;
		}

		if (commonDepth === 0) {
			process.stderr.write("error: --from and --to share no common ancestor\n");
			process.exit(1);
		}

		const commonAncestorNode = fromPath[commonDepth - 1];
		const commonAncestor = $(commonAncestorNode);

		// The children of the common ancestor that contain --from and --to.
		const fromChild =
			commonDepth < fromPath.length ? fromPath[commonDepth] : fromEl[0];
		const toChild = commonDepth < toPath.length ? toPath[commonDepth] : toEl[0];

		// Collect all children of the common ancestor from fromChild to toChild inclusive.
		const fragments = [];
		let found = false;
		let finished = false;
		commonAncestor.children().each(function () {
			if (finished) return;
			if (this === fromChild) found = true;
			if (found) fragments.push($.html($(this)));
			if (this === toChild) finished = true;
		});

		if (!found || !finished) {
			process.stderr.write(
				"error: --from element does not appear before --to in document order within their common ancestor\n",
			);
			process.exit(1);
		}

		// Ancestor chain from the common ancestor up to body.
		const ancestors = buildAncestorChain(commonAncestor);

		const result = buildExtractedDoc($, headContent, ancestors, fragments);
		fs.mkdirSync(path.dirname(output), { recursive: true });
		fs.writeFileSync(output, result, "utf-8");
	} else if (opts.selector) {
		// Single element extraction: --selector
		const target = resolveOne($, opts.selector, "--selector");
		const ancestors = buildAncestorChain(target);
		const fragments = [$.html(target)];

		const result = buildExtractedDoc($, headContent, ancestors, fragments);
		fs.mkdirSync(path.dirname(output), { recursive: true });
		fs.writeFileSync(output, result, "utf-8");
	} else {
		process.stderr.write("error: --selector or --from/--to is required\n");
		process.exit(1);
	}
}

// ---------------------------------------------------------------------------
// Outline command
// ---------------------------------------------------------------------------

const ZERO_WIDTH_RE =
	/^[\s\u200B\u200C\u200D\u2060\uFEFF\u202A-\u202E\u2061-\u2064]*$/;

function isHiddenElement(el) {
	const style = el.attr("style");
	if (!style) return false;
	if (/display\s*:\s*none/i.test(style)) return true;
	if (/max-height\s*:\s*0/i.test(style) && /overflow\s*:\s*hidden/i.test(style))
		return true;
	return false;
}

function isMsoComment(node) {
	if (node.type === "comment") {
		const data = node.data || "";
		return /\[if\s+mso/i.test(data) || /\[endif\]/i.test(data);
	}
	return false;
}

function isTrackingImg(el) {
	const w = parsePxValue(el.attr("width"));
	const h = parsePxValue(el.attr("height"));
	return w && h && w <= 1 && h <= 1;
}

function isEmptySpacer(el, $) {
	const text = el.text();
	return ZERO_WIDTH_RE.test(text) && el.children().length === 0;
}

function isSectionMarker(el) {
	const style = el.attr("style") || "";
	const tag = el[0] && el[0].name;
	if (/max-width\s*:\s*6\d\dpx/i.test(style)) return true;
	if (tag === "table" && el.parent().length && el.parent()[0].name === "body")
		return true;
	return false;
}

function stylePreview(el) {
	const style = el.attr("style");
	if (!style) return "";
	// Extract the most layout-relevant property.
	const maxW = style.match(/max-width\s*:\s*[^;]+/i);
	if (maxW) return maxW[0].trim();
	const w = style.match(/width\s*:\s*[^;]+/i);
	if (w) return w[0].trim();
	const display = style.match(/display\s*:\s*[^;]+/i);
	if (display) return display[0].trim();
	// Truncate to 60 chars.
	return style.length > 60 ? style.slice(0, 57) + "..." : style;
}

function imgDimsLabel(el) {
	const w = el.attr("width");
	const h = el.attr("height");
	if (w && h) return `${w}x${h}`;
	return "";
}

function textPreview(el, $) {
	// Direct text children only (not nested element text).
	let text = "";
	el.contents().each(function () {
		if (this.type === "text") text += this.data;
	});
	text = text.trim();
	if (!text) return "";
	return text.length > 60 ? `"${text.slice(0, 57)}..."` : `"${text}"`;
}

function sectionSummary(el, $) {
	// Scan descendants for first image and first meaningful text.
	const annotations = [];
	let foundImg = false;
	let foundText = false;

	function scan(node) {
		if (foundImg && foundText) return;
		const children = node.children().toArray();
		for (const child of children) {
			if (foundImg && foundText) return;
			const ch = $(child);
			const tag = child.name;
			if (tag === "img" && !foundImg) {
				const w = ch.attr("width");
				const h = ch.attr("height");
				if (w && h && !(parseInt(w, 10) <= 1 && parseInt(h, 10) <= 1)) {
					annotations.push(`img ${w}x${h}`);
					foundImg = true;
				}
			}
			if (!foundText) {
				// Check direct text children of this element.
				let text = "";
				ch.contents().each(function () {
					if (this.type === "text") text += this.data;
				});
				text = text.trim();
				if (text && !ZERO_WIDTH_RE.test(text)) {
					const preview = text.length > 50 ? text.slice(0, 47) + "..." : text;
					annotations.push(`"${preview}"`);
					foundText = true;
				}
			}
			scan(ch);
		}
	}

	scan(el);
	return annotations.length > 0 ? annotations.join(", ") : "";
}

function buildSelectorForSection(el, $) {
	// Build an actual CSS selector by walking the DOM path from <body> to this element.
	const tag = el[0].name;
	const id = el.attr("id");
	if (id) return `#${id}`;

	const parts = [];
	let current = el;
	while (
		current.length &&
		current[0].name &&
		current[0].name !== "body" &&
		current[0].name !== "html"
	) {
		const node = current;
		const t = node[0].name;
		const nid = node.attr("id");
		if (nid) {
			parts.unshift(`#${nid}`);
			break; // id is unique, no need to go higher
		}
		const cls = node.attr("class");
		// Count same-tag siblings before this node to get nth-of-type index.
		let idx = 1;
		let sib = node.prev();
		while (sib.length) {
			if (sib[0].name === t) idx++;
			sib = sib.prev();
		}
		// Count total same-tag siblings to decide if :nth-of-type is needed.
		let totalSameTag = idx;
		sib = node.next();
		while (sib.length) {
			if (sib[0].name === t) totalSameTag++;
			sib = sib.next();
		}
		let part = t;
		if (cls) {
			const first = cls.trim().split(/\s+/)[0];
			part += `.${first}`;
		} else if (totalSameTag > 1) {
			part += `:nth-of-type(${idx})`;
		}
		parts.unshift(part);
		current = current.parent();
	}

	return parts.join(" > ");
}

function cmdOutline(input, opts) {
	const html = fs.readFileSync(input, "utf-8");
	const $ = cheerio.load(html, {
		decodeEntities: false,
		sourceCodeLocationInfo: true,
	});

	const maxDepth = opts.full ? Infinity : opts.depth;
	const showSelectors = opts.selectors;

	function lineNumberFor(el) {
		const loc = el[0] && el[0].sourceCodeLocation;
		return loc ? loc.startLine : null;
	}

	const output = [];
	const sections = [];
	let sectionCount = 0;

	function walk(node, depth, isLastChild) {
		const type = node[0] && node[0].type;
		if (!type) return;

		// Skip comments (including MSO).
		if (type === "comment") return;
		if (type === "text") return;
		if (type !== "tag" && type !== "style" && type !== "script") return;

		const tag = node[0].name;
		const el = node;

		// Skip non-layout elements.
		if (
			tag === "style" ||
			tag === "script" ||
			tag === "meta" ||
			tag === "link" ||
			tag === "head"
		)
			return;

		// Skip hidden elements.
		if (isHiddenElement(el)) return;

		// Skip empty spacers.
		if ((tag === "div" || tag === "span") && isEmptySpacer(el, $)) return;

		// Skip tracking pixels.
		if (tag === "img" && isTrackingImg(el)) return;

		const lineNum = lineNumberFor(el);
		const lineLabel = lineNum ? `L${String(lineNum).padStart(3)}` : "    ";

		// Build tree prefix.
		const isSection = isSectionMarker(el);
		let prefix;
		if (depth === 0 || isSection) {
			prefix = "\u250c"; // ┌
		} else {
			prefix = "\u2502 ".repeat(Math.max(0, depth - 1)) + "\u2514 "; // │ ... └
		}

		// Build description.
		let desc = tag;
		const id = el.attr("id");
		if (id) desc += `#${id}`;
		const cls = el.attr("class");
		if (cls) {
			const classes = cls.trim().split(/\s+/).slice(0, 3).join(".");
			desc += `.${classes}`;
		}

		const stylePrev = stylePreview(el);
		if (stylePrev) desc += `  ${stylePrev}`;

		if (tag === "img") {
			const dims = imgDimsLabel(el);
			if (dims) desc = `img ${dims}`;
		}

		const textPrev = textPreview(el, $);
		if (textPrev) desc += `  ${textPrev}`;

		if (isSection) {
			const summary = sectionSummary(el, $);
			desc += "  [SECTION]";
			if (summary) desc += `  ${summary}`;
			sections.push({ el, index: sectionCount++ });
		}

		// Only print non-section elements when within depth limit.
		const withinDepth = depth < maxDepth;
		if (withinDepth || isSection) {
			output.push(`${lineLabel}  ${prefix} ${desc}`);
		}

		if (withinDepth) {
			// Recurse into children.
			const children = el.children().toArray();
			for (let i = 0; i < children.length; i++) {
				walk($(children[i]), depth + 1, i === children.length - 1);
			}
		} else if (isSection) {
			// Section beyond depth limit - children already summarized via sectionSummary.
			// Recurse to find nested sections but don't print non-section content.
			const children = el.children().toArray();
			for (let i = 0; i < children.length; i++) {
				walk($(children[i]), depth + 1, i === children.length - 1);
			}
		} else {
			// Beyond depth limit, not a section - scan for deeper sections only.
			const children = el.children().toArray();
			for (let i = 0; i < children.length; i++) {
				walk($(children[i]), depth + 1, i === children.length - 1);
			}
		}
	}

	// Start from body children.
	const body = $("body");
	if (body.length) {
		const children = body.children().toArray();
		for (let i = 0; i < children.length; i++) {
			walk($(children[i]), 0, i === children.length - 1);
		}
	}

	process.stdout.write(output.join("\n") + "\n");

	// Selector hints.
	if (showSelectors && sections.length > 0) {
		process.stdout.write("\nSuggested selectors:\n");
		for (const { el, index } of sections) {
			const selector = buildSelectorForSection(el, $);
			const lineNum = lineNumberFor(el);
			const lineLabel = lineNum ? `L${lineNum}` : "";
			process.stdout.write(`  ${lineLabel.padEnd(6)} ${selector}\n`);
		}
	}
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
	const parsed = parseArgs(process.argv);

	if (parsed.command === "prepare") {
		await cmdPrepare(parsed.input, parsed.output, parsed.opts);
	} else if (parsed.command === "extract") {
		cmdExtract(parsed.input, parsed.output, parsed.opts);
	} else if (parsed.command === "outline") {
		cmdOutline(parsed.input, parsed.opts);
	}
}

main().catch((err) => {
	process.stderr.write(`error: ${err.message}\n`);
	process.exit(1);
});
