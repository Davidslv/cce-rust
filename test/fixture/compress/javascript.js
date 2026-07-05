// Parse a currency amount from raw text.
function parseAmount(raw) {
  const cleaned = raw.trim().replace("$", "");
  const value = parseFloat(cleaned);
  return Math.round(value * 100) / 100;
}
