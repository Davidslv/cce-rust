# Login rate limiting

Accounts lock after five failed login attempts within fifteen minutes.
The lockout window resets on a successful login. Locked accounts unlock
automatically after thirty minutes, or immediately via a password reset.

## Why

Credential-stuffing bots try thousands of passwords per hour. A small,
fixed attempt budget makes the attack uneconomical without hurting real
users, who rarely fail more than twice.
