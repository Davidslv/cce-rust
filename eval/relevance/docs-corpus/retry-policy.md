# Retry policy for outbound webhooks

Failed webhook deliveries retry with exponential backoff and jitter:
one second, then two, four, eight, and sixteen — five attempts total.
After the final failure the event parks in the dead-letter queue for
manual replay.

## Idempotency

Every delivery carries a stable idempotency key, so a receiver that got
the first attempt can safely drop the retries.
