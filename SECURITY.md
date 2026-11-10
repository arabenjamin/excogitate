# Security policy

## Reporting a vulnerability

**Don't open a public issue.** Report it privately through GitHub:
**Security → Report a vulnerability** on this repository (a private
security advisory). You'll get an acknowledgement, and the fix is
coordinated in the advisory before anything is public.

Please include what you found, how to reproduce it, and what it affects.

## Scope

Excogitate treats every model output as untrusted input. Reports are
especially welcome where a model, or content it fetched, can:

- reach a tool it wasn't offered;
- send a credential to a host it isn't bound to;
- read or mark private data as public;
- reach private network ranges through `http_request`;
- or exceed a budget without the run saying so.

## Supported versions

Pre-alpha: only `main` is supported until the first release.
