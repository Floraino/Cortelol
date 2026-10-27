# YouTube page downloader feasibility

**Historical assessment:** this report describes the fail-closed state before the
2026-10-27 local installer candidate. The current command contract is in
[INTEGRATION.md](INTEGRATION.md). The direct yt-dlp candidate does not satisfy the
AppContainer and broker gates below; the technical risks remain open for a public
release. Statements below about the command rejecting every URL refer to the
earlier implementation.

Assessment date: 2026-10-27. Target: Windows desktop Tauri. Owner: Rota.

## Decision and evidence boundary

A real arbitrary-channel YouTube page downloader with verifiable control of every
request and descendant process is **not ready to enable in this repository**.
This is not a claim that such a downloader is technically impossible. A brokered
extractor with OS isolation is a concrete engineering candidate, but its Windows
enforcement, runtime compatibility, and extraction coverage have not been proved.
Public visibility cannot guarantee that a video is downloadable without login,
cookies, access-control workarounds, or unsupported formats.

PRD section 11 and the corresponding architecture/design proposal were read before
implementation. The latest instruction requires YouTube-only guidance while the
real URL feature remains pending. Vimeo actions and callable transfers are retired;
previous local files, projects, exports, and receipts are preserved. This bounded
change does not deliver the requested download of any public video from any channel.

The local Python installation contains yt-dlp **2026.8.19**. Its source was inspected
without running extraction. Current upstream documentation/source was also reviewed;
upstream master is moving evidence, not a release dependency pin. No packages were
installed, firewall policy changed, AppContainer launched, or real video downloaded.

## Why the existing prototype cannot provide the guarantee

The earlier Python prototype validates the initial URL and metadata URL, then calls
`YoutubeDL.extract_info`. Its public-DNS precheck is separate from the eventual
connection. It neither pins downstream DNS nor inspects every redirect, manifest,
fragment, metadata request, or runtime fetch. Progress hooks do not bound network
traffic during metadata lookup, and reported file sizes do not account for all
responses, retries, or temporary files. Its six earlier mock tests prove prototype
behavior only, not network containment.

yt-dlp routes ordinary HTTP through a request director that can select multiple
handlers. A custom handler can provide a Rust IPC adapter, but handler preference
alone is insufficient: all fallback handlers must be absent, and the OS must deny
direct networking even if Python or native code bypasses the adapter. This is an
inference from the upstream [request director and handler implementation](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/networking/common.py)
and [YoutubeDL construction](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/YoutubeDL.py).

Additional paths include external FFmpeg/downloaders and JavaScript challenge
runtimes. They execute separately and are not contained by a Python HTTP hook.
An HTTPS CONNECT proxy carries an encrypted tunnel; merely checking its target
does not validate each request/redirect inside it. Environment proxy settings,
`--ignore-config`, and Python socket monkey-patches are configuration controls,
not an OS security boundary. See upstream [external downloaders](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/downloader/external.py)
and the [Deno provider](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/youtube/jsc/_builtin/deno.py).

YouTube extraction currently uses an external JavaScript runtime and EJS solver
components. Upstream supports local EJS packages and optional remote fetching; an
approved runtime/package tuple would need to be pinned, supplied locally, and
run without remote component installation. A compatible solver is an additional
dependency and maintenance commitment, not a bundled feature today. See the
[official EJS setup guide](https://github.com/yt-dlp/yt-dlp/wiki/EJS).

Not loading a browser cookie jar does **not** mean no cookies: the inspected
YouTube extractor initializes anonymous `PREF` and `SOCS` cookies itself. A strict
no-cookie design must reject/remove outgoing Cookie and Authorization headers,
discard Set-Cookie, and validate whether the supported anonymous extraction subset
still works. No cookie-dependent fallback is allowed. See the upstream
[YouTube base extractor](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/extractor/youtube/_base.py).
Plugins also require explicit disabling before initialization; inspect
[plugin discovery](https://github.com/yt-dlp/yt-dlp/blob/master/yt_dlp/plugins.py).

## Candidate architecture: hypothesis, not implemented

1. A fixed, reviewed Python/yt-dlp/EJS runtime runs as an untrusted extraction
   worker. It receives only a canonical YouTube video identity, bounded responses,
   and a private staging capability. No browser profiles, credentials, source
   recordings, user home, arbitrary output paths, or inherited connected sockets
   are available. Disable plugins/config/netrc, automatic updates, remote
   components, generic extractors, shell hooks, and external network downloaders.
2. Windows starts the worker suspended with an AppContainer token with no Internet,
   private-network, or server capabilities. Constrain filesystem ACLs and inherited
   handles; verify the token, integrity level and effective restrictions before
   resuming. Use a Job Object with kill-on-close, bounded resources/process count,
   no breakaway, and verified containment for every allowed child. An EJS child
   must inherit containment; a total child-process ban would break this workflow.
3. A bounded pipe protocol carries requests to Rust. The worker has no HTTP proxy
   listener or network exception. A sole custom request handler converts supported
   extractor requests to IPC; unknown protocols or unsupported operations fail.
   Do not expose arbitrary filesystem or subprocess actions through the broker.
4. Rust owns all external HTTP and DNS: allowlisted YouTube/API/media hosts and
   methods, HTTPS 443, normal TLS hostname/certificate checks, no system proxy,
   no credentials/cookies, bounded bodies/headers/time/request count, and global
   job byte/concurrency budgets. Validate all DNS A/AAAA answers, reject mixed or
   private/reserved answers, pin the selected IP without resolver fallback, and
   verify the peer. Reapply the full policy at each redirect and each manifest,
   fragment, retry, metadata, and player-script request. Redirects cannot expand
   the allowed host/method policy. POST redirect semantics must be explicit.
5. Treat metadata, format lists, subtitles, manifests, filenames, and IPC as
   untrusted. Start with a bounded ordinary-video format subset; reject live,
   playlists, DRM, authentication/age/region/paywall restrictions, and quota
   responses without alternate-account/IP/client workarounds. Rust downloads
   media via the same broker. Any mux/probe process uses validated local files
   only and equivalent isolation, with no remote URL or arbitrary ffmpeg args.
6. Rust validates the completed file through the existing import/FFprobe boundary,
   hashes it, and publishes unique media/receipt paths without overwrite. Receipts
   include canonical video identity, runtime/policy versions, timestamp, user
   declaration and checksum; exclude signed URLs/queries, cookies and raw headers.
   Cancellation, timeout, crash, and worker death terminate the job tree and clean
   only its own partial files. Previously completed sources/exports remain intact.

The existing `network.rs` offers useful DNS pinning, manual redirect, peer-check,
and publication fixtures. It is a single direct-file GET transport: it does not
implement the HTTP metadata/POST/Range/manifest/IPC broker above and must not be
converted by merely adding YouTube hosts to its allowlist.

Microsoft documents AppContainer resource isolation for unpackaged applications,
capability tokens, and Job Objects. These support the candidate design, but do not
prove our complete worker chain is contained. Job Objects alone manage lifetime
and resources; they do not validate network traffic. See
[AppContainer for legacy apps](https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-for-legacy-applications-),
[security capabilities](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-security_capabilities),
and [Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).

Effective Windows network enforcement also depends on WFP/firewall policy. Original
[Project Zero research](https://projectzero.google/2021/08/understanding-network-access-windows-app.html)
documents policy interactions and the DNS-client-service path. This historical
research is a reason to test current supported Windows configurations; it does
not establish that a particular historical bypass works on today's machines.
Do not claim containment merely because token creation succeeded. If isolation
or effective-policy checks cannot establish the required boundary, fail closed.
An explicit WFP/service or stronger isolation alternative may require elevated
installation and packaging decisions; none is introduced by this assessment.

Linux could use an empty network namespace, no inherited network sockets,
restricted mounts, dropped capabilities/no_new_privs, syscall restrictions, and
pipe IPC. Namespace availability and DNS service sockets must be checked. This
does not validate Windows or imply Linux release support. See
[Linux network namespaces](https://man7.org/linux/man-pages/man7/network_namespaces.7.html).

## Concrete validation plan and stop conditions

| Step | Required evidence before advancing |
| --- | --- |
| 1. Windows isolation spike outside the shipping route | A fixed worker and EJS child cannot reach TCP/UDP IPv4/IPv6, loopback/LAN/link-local/metadata/public addresses, OS DNS/RPC, or external proxies except through the authorized pipe broker; token/ACL/handle and process-tree evidence is captured. |
| 2. Adversarial process tests | Direct Python sockets, native networking, subprocess/PowerShell/cmd attempts, child breakaway, inherited-handle abuse, plugin/import/config injection and filesystem traversal cannot obtain extra capabilities. Verify AppContainer policy failures cause refusal, not ordinary process launch. |
| 3. Broker tests | Assert no connection for unsafe/mixed/rebinding DNS, unsafe redirects, alternate ports/schemes, credential headers, unexpected hosts, malformed IPC, redirects with bodies, manifest/key/fragment escape and cross-host POST. Test TLS/SNI/peer checks with a controlled transport, byte/time/request limits and cancellation. |
| 4. Local persistence and cleanup | Cancellation/crash at every stage preserves sentinel source/receipt/export files; symlinks/reparse points, collisions, partial receipt publication, corrupt/oversized media and malformed metadata cannot publish or overwrite a source. |
| 5. Authorized integration and clean Windows acceptance | Explicitly permitted real-video fixtures demonstrate the supported no-cookie format subset, repeat after restart, and fail transparently for unavailable/restricted videos. Verify installed runtime hashes/versions/licenses and no background dependency fetches. |
| 6. Enablement decision | Record the supported method, permissions, limits and remaining failures in PRD/architecture. Independent review must cover the effective Windows boundary and all descendants. Only then expose a new typed URL job API. |

No timeline or universal-video success rate is established. A failed gate leaves
the feature unsupported. The shipping change below is independently verifiable
because it performs no remote extraction or transfer at all.

## Current implemented contract

- `download_video` keeps its registered Tauri name and legacy payload/result shape,
  but rejects every URL before URL parsing, DNS, filesystem access or child launch.
  Rights/method flags cannot override it. No request URL appears in its error.
- `network.rs` is compiled only under `cfg(test)`. Earlier transport fixtures remain
  available as research regression tests; there is no production transport fallback.
- `open_acquisition_help` accepts only `youtubeStudio` and `googleTakeout` and opens
  fixed URLs after a click. Vimeo help topics fail deserialization. The external
  browser operates independently; Cortelol does not inspect or collect its session.
- The UI has no URL form, transfer start, download progress, Vimeo action or metadata
  fetch. It explicitly explains that public YouTube URL downloads are unavailable.
  Existing local import and its busy/error handling remain available.
- Python's default entry point rejects before DNS/path access/importing yt-dlp;
  only an explicitly injected fake factory exercises retained offline fixtures.
  The PowerShell wrapper rejects without resolving paths or starting Python.
- No source, export or receipt cleanup/migration is added. `App.tsx`, `lib.rs`,
  shared catalogs and root documents are owned by other teammates and untouched.

Catalog keys and the exact shell contract are recorded in [INTEGRATION.md](INTEGRATION.md).
The PRD's separate method/permission boundary remains pending. This report does
not replace it with a network-safety assertion. Official
[YouTube creator download guidance](https://support.google.com/youtube/answer/56100?hl=en)
describes Studio/Takeout for the uploader's own videos; guidance is not an arbitrary
channel extraction API.

## Verification snapshot

On 2026-10-27, after the bounded revision:

- `cargo check --manifest-path src-tauri/Cargo.toml --lib --offline`: passed,
  including the non-test configuration with no compiled download transport.
- `cargo test --manifest-path src-tauri/Cargo.toml download:: --offline`: 20 passed,
  including source/receipt/export preservation, every-source command rejection,
  strict YouTube-only help topics, and retained transport security regressions.
- `python -I scripts/download_video.py --self-test`: 9 passed. CLI tests assert
  no DNS lookup, destination resolution, or child-process launch for YouTube/Vimeo.
- PowerShell parser: zero executable command AST nodes in the wrapper; explicit
  YouTube and Vimeo invocations both rejected with a nonexistent destination,
  without running Python, resolving paths, or executing any network operation.
- `npm run build`: passed. Vite reports the existing JavaScript chunk-size warning.

Rust reports existing dead-code warnings outside this production boundary and
two expected test-only warnings for the retained unused live transport types.
No live extraction, OS containment test, or installed desktop interaction was run.
The catalog owner still needs to verify all locales against the three new keys.
