# Search relevance tuning notes

The ranker fuses keyword and vector candidate lists with reciprocal
rank fusion, then blends a confidence score on top. Keyword weight is
boosted for code-lookup intent queries.

## Changing the weights

Any weight change must show its effect on the labeled fixture sets
before it merges: run the relevance harness and compare the before and
after scores per query.
