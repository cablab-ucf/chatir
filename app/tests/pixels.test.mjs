// node --test app/tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { pixels } from "../ui/pixels.js";

const bytes = (...parts) => {
  const b = new Uint8Array(64);
  let i = 0;
  for (const p of parts) {
    for (const x of typeof p === "string" ? [...p].map((c) => c.charCodeAt(0)) : p) b[i++] = x;
  }
  return b;
};
const be32 = (n) => [n >>> 24, n >>> 16 & 255, n >>> 8 & 255, n & 255];
const le16 = (n) => [n & 255, n >>> 8];
const le24 = (n) => [n & 255, n >>> 8 & 255, n >>> 16];

test("each format's header gives its size", () => {
  const png = (w, h) => bytes([0x89], "PNG\r\n\x1a\n", be32(13), "IHDR", be32(w), be32(h));
  assert.equal(pixels(png(640, 480)), 640 * 480);
  assert.equal(pixels(png(30000, 30000)), 9e8, "a bomb is seen as one");
  assert.equal(pixels(bytes("GIF89a", le16(300), le16(200))), 60000);
  assert.equal(pixels(bytes("BM", Array(16).fill(0), le16(10), [0, 0], le16(65526), [255, 255])), 100);
  const webp = (k, ...rest) => bytes("RIFF", [0, 0, 0, 0], "WEBP", k, [0, 0, 0, 0], ...rest);
  assert.equal(pixels(webp("VP8X", [0, 0, 0, 0], le24(99), le24(49))), 5000);
  const l = (100 - 1) | (50 - 1) << 14;
  assert.equal(pixels(webp("VP8L", [0x2f], le24(l), [l >>> 24])), 5000);
  assert.equal(pixels(webp("VP8 ", [0, 0, 0, 0x9d, 0x01, 0x2a], le16(100), le16(50))), 5000);
  const jpeg = bytes([0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xff, 0xc2, 0, 11, 8],
    [0x0f, 0xa0, 0x17, 0x70]);
  assert.equal(pixels(jpeg), 4000 * 6000, "past APP0 and a fill byte, a progressive frame");
  const avif = bytes([0, 0, 0, 20], "ftypavif", [0, 0, 0, 0, 0, 0, 0, 20], "ispe",
    [0, 0, 0, 0], be32(800), be32(600));
  assert.equal(pixels(avif), 480000);
});

test("what the header does not say is too big", () => {
  assert.equal(pixels(new Uint8Array(8)), Infinity);
  assert.equal(pixels(bytes("<svg>")), Infinity);
  assert.equal(pixels(bytes([0xff, 0xd8, 0xff, 0xe0, 0, 200])), Infinity, "cut short");
});
