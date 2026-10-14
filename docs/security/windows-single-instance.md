# Windows single-instance security

Status: implemented fork candidate; automated regressions validated on the repaired
implementation, with final cleanup validation recorded in the contribution draft.
Manual Windows security-boundary and shell verification remains outstanding.
Based on upstream `87b448b6`; compatible by inspection and merge simulation with
`0a098648` (2026-10-12). This feature is not merged or released.

## Scope and threat model

This follows [echelon's invitation in #616](https://github.com/storytold/pdfcraft/pull/616#issuecomment-6096621666)
to harden the AF_UNIX socket merged in [#715](https://github.com/storytold/pdfcraft/pull/715),
based on [#602](https://github.com/storytold/pdfcraft/pull/602).

Protect document paths and queue admission against another unprivileged Windows
account and malformed local clients. Treat all socket bytes, endpoint files and
environment values as untrusted. Administrators, SYSTEM and malicious processes
under the same Windows user are outside the authentication boundary: they can
access that user's files and data-protection credentials. Another session of the
same user is not a separate security principal.

## Transport, election and routing

The existing `uds_windows` AF_UNIX transport stays in place. No network listener,
named-pipe replacement, document loader or new dependency package is introduced.
Windows enforces socket-path and parent-directory permissions. The wrapper has no
peer-token API and does not independently verify an owner-only DACL.

The Windows known-folder API, using the process token, supplies LocalAppData; the
process token supplies the session ID. `LOCALAPPDATA` and `SESSIONNAME` cannot
redirect routing. Installed and portable launches have distinct endpoint names;
portable copies in the same user/session retain upstream's shared portable group,
not isolation by portable folder.

The existing startup file lock serializes connection, stale-endpoint cleanup,
binding and listener startup. It is never deleted and Windows releases it on
process termination. Lock failure falls back to a separate window. Local-drive
parent and lock checks reject reparse points; these checks are best effort, not
race-free directory handles. Stale cleanup only follows a missing/refused
connection, never a timeout or arbitrary connection failure. Regular files at the
endpoint are left untouched. Listener ownership remains outside renderer retries.

## Authentication and protocol

The primary sends a fresh random 32-byte nonce protected with Windows
`DataProtectionProvider("LOCAL=user")`. The client decrypts that challenge and
returns it in a user-protected request containing document paths and its own fresh
nonce. The primary verifies its nonce before admitting the entire batch, then
returns a protected acknowledgment binding both nonces. The client verifies both
before reporting a completed handoff.

This proves possession of the Windows user's cryptographic context, not process
identity or logon-session membership. No reusable key file is needed. Unprotecting
a blob alone is not authentication: Windows can also decrypt machine-scoped blobs.
The secret primary challenge and sealed client nonce prevent such a fake challenge
from producing an authenticated request or forged acknowledgment. A ciphertext
relay can deliver the intended request, but cannot read or modify its paths.

Frames have an explicit big-endian length checked before allocation. Messages
require version 1 and exact request kinds/keys; only document paths are forwarded.
Paths must be absolute, nonempty and NUL-free. Requests with malformed JSON,
truncated frames, unknown versions or unexpected fields are refused.

| Resource | Bound |
|---|---|
| Plaintext request | 1 MiB |
| Protected frame | 1 MiB + 4 KiB |
| Protected challenge/acknowledgment frame | 4 KiB |
| Paths per batch / bytes per path | 256 / 32 KiB |
| Receiver workers / active and waiting connections | 4 / 8 |
| Pending batches / paths / path bytes | 16 / 1,024 / 4 MiB |
| Election, connect wait and exchange deadlines | 5 seconds per stage |

A total exchange deadline includes queue wait, nonblocking IO and asynchronous
protection waits; trickled bytes cannot renew it. Admission checks the entire
batch atomically. Queue saturation refuses admission before acknowledgment.
Shutdown signals workers and joins them; their IO and async waits observe stop.

## Compatibility and delivery limits

Ordinary file-only launches use the existing OS-event/document-opening route.
`--control`, other launch options, `--new-window`, image imports and empty launches
keep upstream's independent-window behavior. Failures open a separate window,
without a fatal handoff dialog or insecure protocol downgrade. Existing document,
tab, workspace, session-restoration and renderer-fallback logic is unchanged.

An acknowledgment means admitted to the primary's in-memory queue, not durable
storage or successful document loading. A primary crash can lose queued paths.
Lost acknowledgments retain upstream's fallback and may duplicate document opens.
Old protocol/endpoint versions also fall back; restart older instances on upgrade.

A Winsock connect has no cancellation API in this wrapper: one launch-local
thread can outlive the caller's timeout until the OS returns. Late streams are
dropped and the socket is not deleted. Synchronous OS activation cannot be
forcibly interrupted either; bounded shutdown tests cover normal supported OS
behavior, not a stuck OS API. Filesystem manipulation and clients occupying the
bounded slots can still cause denial of service. No remote-peer rejection or
race-free filesystem guarantee is claimed.

macOS Apple Events and Linux/FreeBSD/web behavior are unchanged. Linux/FreeBSD
single-instance forwarding, macOS direct-executable forwarding, tab detaching
and Combine Files are not implemented by this contribution.

## Verification and remaining manual work

| Evidence | What it establishes |
|---|---|
| Portable automated tests | Framing/truncation, schemas, versions, nonce binding, path limits, atomic queue limits/draining, connection reservations, total deadlines and launch-option bypasses |
| Native Windows automated tests | Real protection/tampering and sockets, Unicode/spaced paths, multiple documents, simultaneous thread/process elections, hostile messages, stalled clients, queue-full fallback, uncertain acknowledgments, crash recovery, normal shutdown and environment-independent routing |
| Code/API inspection | User-protected nonce protocol, OS profile/session lookup, inherited filesystem permissions, unchanged startup/UI integration and lack of externally reachable network listener |
| Manual Windows testing still required | Separate accounts, console/RDP sessions, mixed elevation, modified ACLs/reparse points, Explorer double-click/Open With, multiple-file associations, minimized focus, portable/removable/network-drive launches |

Same-account automated tests do not verify different-account, elevation or session
boundaries. Session-specific names route legitimate launches but do not prevent
same-user access across sessions. Window focus is best effort. Engine fuzzing tests
PDF processing, not the IPC handshake; malformed-protocol tests cover that path.

### Crypto factory lifetime

A cold-process regression reproduced a successful first crypto round trip followed
by an access violation on activation after a temporary COM guard was dropped.
The Windows bindings cache agile activation factories for process lifetime;
`CoUninitialize` can invalidate those references despite their retained COM count.
The fix uses the pinned `windows-core` automatic activation, which establishes
process-lived MTA usage when needed. Providers and async operations remain local
to their calling threads. Repeated cold crypto initialization/use/drop and real
process startup remain regression tests. Native tests passed after this repair.

## Sources

- [Microsoft: Windows AF_UNIX and pathname permissions](https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/)
- [Microsoft: file security and inherited descriptors](https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights)
- [Microsoft: DataProtectionProvider](https://learn.microsoft.com/en-us/uwp/api/windows.security.cryptography.dataprotection.dataprotectionprovider)
- [Microsoft: peer impersonation supports IP sockets](https://learn.microsoft.com/en-us/windows/win32/api/ws2tcpip/nf-ws2tcpip-wsaimpersonatesocketpeer)
- [Microsoft: cached factories invalidated by COM teardown](https://devblogs.microsoft.com/oldnewthing/20211105-00/?p=105878/)
