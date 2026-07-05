# Parse a currency amount from raw text.
def parse_amount(raw)
  cleaned = raw.strip.delete("$")
  value = cleaned.to_f
  value.round(2)
end
