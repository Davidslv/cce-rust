def parse_amount(raw):
    """Parse a currency amount from raw text."""
    cleaned = raw.strip().replace("$", "")
    value = float(cleaned)
    return round(value, 2)
