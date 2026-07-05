/* Parse a currency amount from raw text. */
double parse_amount(const char *raw) {
    double value = atof(raw);
    double cents = value * 100.0;
    return cents / 100.0;
}
