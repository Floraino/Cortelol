# Windows downloader isolation spike

Date: 2026-10-27. This experiment is standalone and is not registered by the
desktop downloader. It makes no video or HTTP download and changes no firewall,
loopback exemption, or machine policy. It creates a unique per-user AppContainer
profile for each run and deletes it before returning.

## Run

From the repository root on Windows, compile `spike.rs` with `rustc --edition
2024 src-tauri/src/download/isolation/spike.rs -o <temporary-path>\spike.exe`.
Then run `<temporary-path>\spike.exe` and `<temporary-path>\spike.exe
--simulate-setup-failure`. The second command must exit nonzero. Delete the
compiled executable afterward. Running in an outer filesystem sandbox can deny
profile creation; that is an expected fail-closed outcome, not an isolation pass.

## Mechanism

- Create a unique AppContainer profile with zero capabilities. Copy only the
  fixed test binary into its profile Temp directory. The worker gets a small
  explicit environment block and inherits no handles.
- Create a Job Object with kill-on-close, a four-process limit and a 256 MiB
  per-process memory limit. Neither breakaway flag is set.
- Start the worker suspended with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`.
  Verify its effective token is an AppContainer, its SID matches the new profile,
  it has zero capability SIDs, and it is in the job. On any failure, terminate
  it without resuming. An injected failure exercises that branch and checks
  that the worker did not create its marker.
- Once resumed, the worker tests connection to live local TCP listeners on
  IPv4 and IPv6, sends UDP to a local listener, attempts a public TCP connection,
  reads a sentinel outside its profile, writes a marker inside the profile,
  starts a child that checks its own AppContainer token and Job membership, and
  attempts a child start with `CREATE_BREAKAWAY_FROM_JOB`. The supervisor checks
  that no local listener received traffic. Worker exit bits fail the test.
- The supervisor removes the test sentinel, marker and copied executable,
  deletes the profile, and checks that its profile folder no longer exists on
  both normal and injected-failure runs.

## Native result on this machine

Windows 11 build 26200, 2026-10-27. `rustc --edition 2024` compiled the source.
The normal run exited 0 and printed token/Job, probe-denial, and profile-removal
passes. The injected setup failure exited 1, with no worker marker, and printed
a profile-removal pass. An initial run in the outer workspace sandbox failed at
`CreateAppContainerProfile` (HRESULT `0x80070002`) and did not launch a worker;
the native runs were performed with permission outside that sandbox. A UDP
`send_to` success by itself was observed and was not counted as delivery; the
test now checks a live local receiver instead.

## Remaining gates

This is evidence for one host configuration, not proof of the required complete
boundary. The following are unverified and block enabling the downloader:

- Effective WFP policy and traffic traces for direct public, LAN, link-local,
  metadata, IPv6 public, DNS-client service/RPC and proxy paths. A failed TCP
  connection to one public address is not proof of an OS policy denial. DNS
  resolution can return cached/service data, so its result is not an isolation
  assertion here. The local UDP receiver checks delivery only on loopback.
- File ACL inspection across user profiles, browser data, named pipes and other
  sensitive locations; inherited/duplicated handle abuse; arbitrary child
  creation via COM/WMI and native APIs; Job kill-on-close against a live
  descendant and crash/cancellation paths; resource limits beyond this probe.
- The pinned Python/yt-dlp/EJS/FFmpeg worker, its exact process tree, private
  broker IPC, and any request broker. No actual extractor has run in this spike.
- Adversarial tests on clean supported Windows configurations, including the
  configurations used for a release. No production command should use this
  code before those gates pass and the integration is independently reviewed.

API references: [Microsoft AppContainer launch](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer),
[AppContainer folder cleanup](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-getappcontainerfolderpath),
[Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects),
and [Windows Firewall AppContainer diagnostics](https://learn.microsoft.com/en-us/windows/security/operating-system-security/network-security/windows-firewall/troubleshooting-uwp-firewall).
