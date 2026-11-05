# Security

## Trust model

goz has the same trust model as Everything. Stated plainly:

- The daemon runs **elevated** and builds its index from a raw NTFS MFT read, which bypasses per-file NTFS ACLs. The index therefore contains the name, size, and timestamps of every file on every tracked volume, regardless of who can normally read them.
- The query pipe (`\\.\pipe\goz-v1`) grants **Authenticated Users** read/write, so any authenticated local user can query the index. It exposes only that metadata, never file contents. On a shared machine, treat filenames as visible to all local users.
- Any such user can also keep the daemon busy. A query searches each volume for at most 10 s, under a lock the index's updates and the other queries wait for, and stops sooner when its client disconnects with no further request pending, so one user's queries delay the rest by seconds at a time, not minutes.
- Conversely, the client verifies the pipe server's owner SID is Local System or the built-in Administrators group before sending anything, and fails closed. A non-elevated process cannot squat the pipe name and impersonate the daemon. The `--insecure-no-server-check` flag opts out of that check and is a development escape hatch only.

## Reporting a vulnerability

Please report security issues privately rather than opening a public issue, using GitHub's [private vulnerability reporting](https://github.com/mustafaahci/goz/security/advisories/new) (the "Report a vulnerability" button under the repository's Security tab). You will get an acknowledgement, and a fix or mitigation plan once the report is confirmed.
