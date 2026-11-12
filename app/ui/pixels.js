// Width × height from an image's header, before any decoder sees it: a few
// MB of PNG can decode to gigabytes and take a phone's page down with it.
// Infinity when the header does not say (or not within `b`).

export function pixels(b) {
  const at = (i, s) => [...s].every((c, j) => b[i + j] === c.charCodeAt(0));
  const be16 = (i) => b[i] << 8 | b[i + 1];
  const le16 = (i) => b[i] | b[i + 1] << 8;
  const le24 = (i) => le16(i) | b[i + 2] << 16;
  const be32 = (i) => (b[i] << 24 >>> 0) + (b[i + 1] << 16 | be16(i + 2));
  const le32 = (i) => Math.abs(le24(i) | b[i + 3] << 24);
  if (b.length < 30) return Infinity;
  if (at(1, "PNG")) return be32(16) * be32(20);
  if (at(0, "GIF")) return le16(6) * le16(8);
  if (at(0, "BM")) return le32(18) * le32(22);
  if (at(0, "RIFF") && at(8, "WEBP")) {
    if (at(12, "VP8X")) return (1 + le24(24)) * (1 + le24(27));
    if (at(12, "VP8L")) {
      const v = le24(21) + b[24] * 2 ** 24;
      return (1 + (v & 0x3fff)) * (1 + (v >>> 14 & 0x3fff));
    }
    if (at(12, "VP8 ")) return (le16(26) & 0x3fff) * (le16(28) & 0x3fff);
    return Infinity;
  }
  if (b[0] === 0xff && b[1] === 0xd8) {
    // JPEG: segments to the frame's header (SOF0-15, but DHT, JPG, DAC).
    for (let i = 2; i + 9 < b.length;) {
      if (b[i] !== 0xff) return Infinity;
      const m = b[i + 1];
      if (m === 0xff) i++;
      else if (m >= 0xc0 && m <= 0xcf && m !== 0xc4 && m !== 0xc8 &&
        m !== 0xcc) return be16(i + 5) * be16(i + 7);
      else i += 2 + be16(i + 2);
    }
    return Infinity;
  }
  if (at(4, "ftyp")) {
    // AVIF: its largest image (`ispe`), a grid's whole among them.
    let n = 0;
    for (let i = b.indexOf(0x69); i >= 0 && i + 16 <= b.length;
      i = b.indexOf(0x69, i + 1)) {
      if (at(i, "ispe")) n = Math.max(n, be32(i + 8) * be32(i + 12));
    }
    return n || Infinity;
  }
  return Infinity;
}
