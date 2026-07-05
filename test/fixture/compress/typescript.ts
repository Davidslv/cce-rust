/** Parse a currency amount from raw text. */
export function parseAmount(raw: string): number {
  const cleaned = raw.trim().replace("$", "");
  const value = parseFloat(cleaned);
  return Math.round(value * 100) / 100;
}
