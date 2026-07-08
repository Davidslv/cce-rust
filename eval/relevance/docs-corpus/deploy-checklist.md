# Deployment checklist

Before rolling out a release:

1. Run the database migrations against a staging copy first.
2. Confirm the feature flags default to off for new code paths.
3. Watch the error dashboard for ten minutes after each canary step.
4. Keep the previous artifact ready for an instant rollback.

## Rollback rule

If the error rate doubles within the watch window, roll back first and
diagnose second. A rollback is never a failure; a slow one is.
