/// Parse a currency amount from raw text.
pub fn parse_amount(raw: &str) -> f64 {
    let cleaned = raw.trim().replace('$', "");
    let value: f64 = cleaned.parse().unwrap_or(0.0);
    (value * 100.0).round() / 100.0
}
