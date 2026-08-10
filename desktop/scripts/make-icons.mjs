// scripts/make-icons.mjs — regenerate the app icon set from code.
//
// Run: node scripts/make-icons.mjs
//
// The mark is a rounded indigo square with a white "W", drawn analytically and
// 4x supersampled, then box-downsampled to each size. Keeping the generator in
// the repo means the icons have provenance: nobody has to wonder where a
// checked-in binary came from or how to nudge it.
//
// This is a one-off asset step, not part of the app's build — the frontend
// still has no build step at all.

import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const OUT_DIR = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "icons");

// Matches --accent / --accent-hover in views/theme.js.
const TOP = [150, 162, 255];
const BOTTOM = [90, 106, 232];
const GLYPH = [255, 255, 255];
const CORNER_RADIUS = 0.235; // fraction of the side
const STROKE = 0.108;

// A "W" as four joined segments in a unit box.
const W_POINTS = [
  [0.215, 0.285],
  [0.355, 0.725],
  [0.5, 0.45],
  [0.645, 0.725],
  [0.785, 0.285],
];

const SIZES = [
  ["icon.png", 512],
  ["32x32.png", 32],
  ["128x128.png", 128],
  ["128x128@2x.png", 256],
];

function distanceToSegment(px, py, [ax, ay], [bx, by]) {
  const dx = bx - ax;
  const dy = by - ay;
  const lengthSquared = dx * dx + dy * dy;
  const t = lengthSquared === 0 ? 0 : Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / lengthSquared));
  const cx = ax + t * dx;
  const cy = ay + t * dy;
  return Math.hypot(px - cx, py - cy);
}

// Signed distance to a rounded square inscribed in the unit box.
function roundedSquareDistance(px, py) {
  const half = 0.5;
  const radius = CORNER_RADIUS;
  const qx = Math.abs(px - 0.5) - (half - radius);
  const qy = Math.abs(py - 0.5) - (half - radius);
  const outside = Math.hypot(Math.max(qx, 0), Math.max(qy, 0));
  return outside + Math.min(Math.max(qx, qy), 0) - radius;
}

function sample(px, py) {
  if (roundedSquareDistance(px, py) > 0) return null;

  let glyph = Infinity;
  for (let index = 0; index < W_POINTS.length - 1; index += 1) {
    glyph = Math.min(glyph, distanceToSegment(px, py, W_POINTS[index], W_POINTS[index + 1]));
  }
  if (glyph <= STROKE / 2) return GLYPH;

  const mix = py;
  return [
    Math.round(TOP[0] + (BOTTOM[0] - TOP[0]) * mix),
    Math.round(TOP[1] + (BOTTOM[1] - TOP[1]) * mix),
    Math.round(TOP[2] + (BOTTOM[2] - TOP[2]) * mix),
  ];
}

function render(size, supersample = 4) {
  const rgba = Buffer.alloc(size * size * 4);
  const step = 1 / (size * supersample);
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let red = 0;
      let green = 0;
      let blue = 0;
      let alpha = 0;
      for (let sy = 0; sy < supersample; sy += 1) {
        for (let sx = 0; sx < supersample; sx += 1) {
          const px = (x * supersample + sx + 0.5) * step;
          const py = (y * supersample + sy + 0.5) * step;
          const color = sample(px, py);
          if (!color) continue;
          red += color[0];
          green += color[1];
          blue += color[2];
          alpha += 1;
        }
      }
      const offset = (y * size + x) * 4;
      if (alpha === 0) continue;
      // Un-premultiply: average only over the covered samples so edges keep
      // their true hue instead of fading toward black.
      rgba[offset] = Math.round(red / alpha);
      rgba[offset + 1] = Math.round(green / alpha);
      rgba[offset + 2] = Math.round(blue / alpha);
      rgba[offset + 3] = Math.round((alpha / (supersample * supersample)) * 255);
    }
  }
  return rgba;
}

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let index = 0; index < 256; index += 1) {
    let value = index;
    for (let bit = 0; bit < 8; bit += 1) {
      value = value & 1 ? 0xedb88320 ^ (value >>> 1) : value >>> 1;
    }
    table[index] = value;
  }
  return table;
})();

function crc32(buffer) {
  let crc = -1;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ -1) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function encodePng(size, rgba) {
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // RGBA
  // 10..12: compression, filter, interlace — all 0.

  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y += 1) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

mkdirSync(OUT_DIR, { recursive: true });
for (const [name, size] of SIZES) {
  const target = join(OUT_DIR, name);
  writeFileSync(target, encodePng(size, render(size)));
  console.log(`wrote ${target} (${size}x${size})`);
}
