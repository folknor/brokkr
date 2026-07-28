#!/usr/bin/env node
"use strict";

// Offline smoke test for prepare.js. Exercises the corpus-fidelity
// behaviors (HARNESS-IMPROVEMENTS.md items 10-14) against a generated
// fixture: author-attr preservation, data-URI normalization, font-family
// rewriting, inline-run whitespace, and the img background-color hack
// regex. Run: node smoke.js. Exits non-zero on the first failure and
// leaves smoke-tmp/ behind for inspection; cleans up on success.

const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const { PNG } = require("pngjs");
const { imageSize } = require("image-size");

const tmpDir = path.join(__dirname, "smoke-tmp");
fs.rmSync(tmpDir, { recursive: true, force: true });
fs.mkdirSync(tmpDir, { recursive: true });

function pngDataUri(width, height, rgb) {
	const png = new PNG({ width, height });
	for (let i = 0; i < width * height; i++) {
		png.data[i * 4] = rgb[0];
		png.data[i * 4 + 1] = rgb[1];
		png.data[i * 4 + 2] = rgb[2];
		png.data[i * 4 + 3] = 0xff;
	}
	const buf = PNG.sync.write(png, { colorType: 2 });
	return `data:image/png;base64,${buf.toString("base64")}`;
}

const redUri = pngDataUri(40, 30, [0xff, 0, 0]);
const longRun = `<span>${"a".repeat(60)}</span><span>${"b".repeat(60)}</span>`;

const input = `<!DOCTYPE html>
<html>
<head>
<style>
@font-face { font-family: 'BrandFont'; src: url('data:font/woff2;base64,AAAA'); }
body { font-family: Arial, sans-serif !important; }
p { font-family: Georgia; }
.bigimg { background-color: red; }
.desktop img { background-color: blue; }
img { background-color: #d0d0d0; }
</style>
</head>
<body>
<img src="${redUri}" width="20">
<img src="${redUri}">
<table><tr>
<td background="https://cdn.example.com/banner.jpg">points banner</td>
<td background="data:image/png;base64,AAAA">stable banner</td>
<td style="font-family:Courier">styled</td>
</tr></table>
<p>${longRun}</p>
<div><b>x</b> <i>y</i><div>block</div></div>
</body>
</html>
`;

const inputPath = path.join(tmpDir, "input.html");
const outputPath = path.join(tmpDir, "output.html");
const fontPath = path.join(tmpDir, "ahem.ttf");
fs.writeFileSync(inputPath, input);
// injectAhemFont only base64s the bytes; content is irrelevant here.
fs.writeFileSync(fontPath, Buffer.from("not a real font"));

execFileSync(
	process.execPath,
	[path.join(__dirname, "prepare.js"), "prepare", inputPath, outputPath, "--ahem-font", fontPath],
	{ stdio: ["ignore", "inherit", "inherit"] },
);

const out = fs.readFileSync(outputPath, "utf-8");

let failures = 0;
function check(name, ok) {
	if (ok) {
		process.stdout.write(`ok   ${name}\n`);
	} else {
		failures++;
		process.stdout.write(`FAIL ${name}\n`);
	}
}

// Item 11: data-URI images are re-encoded to gray placeholders of the
// same natural size.
check("data-URI src replaced", !out.includes(redUri));
const srcs = [...out.matchAll(/src="(data:image\/png;base64,[^"]+)"/g)].map((m) => m[1]);
check("both imgs still data-URI PNGs", srcs.length === 2);
const dims = srcs.map((s) => imageSize(Buffer.from(s.split(",")[1], "base64")));
check(
	"placeholders keep natural size 40x30",
	dims.every((d) => d.width === 40 && d.height === 30),
);

// Item 10: author width attr survives, no height invented next to it;
// the attr-less img gets both attrs from the natural size.
check("author width=20 preserved", /<img[^>]*width="20"(?![^>]*height=)/.test(out));
check("attr-less img gains 40x30", /<img[^>]*width="40"[^>]*height="30"/.test(out));

// Item 12: font-family declarations rewritten, importance preserved,
// author @font-face gone, ahem @font-face present.
check("author @font-face removed", !out.includes("BrandFont"));
check("ahem @font-face injected", out.includes("font-family: 'ahem';"));
check(
	"important font decl rewritten",
	out.includes("font-family: 'ahem' !important") && !out.includes("Arial"),
);
check("plain font decl rewritten", !out.includes("Georgia"));
check(
	"inline style font rewritten",
	!out.includes("Courier") && /style="[^"]*font-family: 'ahem'/.test(out),
);

// Item 13: long inline runs stay verbatim - adjacent spans must not gain
// a newline between them; the deliberate space between <b> and <i>
// survives in the mixed-content run.
check("long inline run verbatim", out.includes("</span><span>"));
check("inter-inline space survives", out.includes("</b> <i>"));

// Legacy `background` attribute: remote URL stripped (Chrome would
// fetch it at capture time), data URI left alone.
check("remote background attr stripped", !out.includes("cdn.example.com/banner.jpg"));
check(
	"data-URI background attr survives",
	out.includes('background="data:image/png;base64,AAAA"'),
);

// Item 14: bare img background-color hack stripped, lookalikes intact.
check("bare img bg rule stripped", !out.includes("#d0d0d0"));
check(".bigimg rule survives", out.includes(".bigimg { background-color: red; }"));
check(".desktop img rule survives", out.includes(".desktop img { background-color: blue; }"));

if (failures > 0) {
	process.stdout.write(`\n${failures} failure(s); output kept at ${outputPath}\n`);
	process.exit(1);
}
fs.rmSync(tmpDir, { recursive: true, force: true });
process.stdout.write("\nsmoke: all checks passed\n");
