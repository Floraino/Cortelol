//! Standalone Windows isolation experiment. Compile with `rustc spike.rs -o spike.exe`.
//! This file is deliberately not registered in the Tauri download module.
#![cfg(windows)]

use std::{env, ffi::c_void, fs, io, mem::{size_of, zeroed}, net::{TcpListener, UdpSocket}, os::windows::{ffi::OsStrExt, process::CommandExt}, path::{Path, PathBuf}, ptr::{null, null_mut}, process::Command, time::Duration};

type Handle = *mut c_void;
type Dword = u32;
type Bool = i32;
const CREATE_SUSPENDED: Dword = 0x4;
const CREATE_NO_WINDOW: Dword = 0x0800_0000;
const CREATE_UNICODE_ENVIRONMENT: Dword = 0x400;
const CREATE_BREAKAWAY_FROM_JOB: Dword = 0x0100_0000;
const EXTENDED_STARTUPINFO_PRESENT: Dword = 0x80000;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x20009;
const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: Dword = 0x8;
const JOB_OBJECT_LIMIT_PROCESS_MEMORY: Dword = 0x100;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: Dword = 0x2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const TOKEN_QUERY: Dword = 0x8;
const TOKEN_IS_APP_CONTAINER: Dword = 29;
const TOKEN_CAPABILITIES: Dword = 30;
const TOKEN_APP_CONTAINER_SID: Dword = 31;
const WAIT_OBJECT_0: Dword = 0;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

#[repr(C)]
struct SecurityCapabilities { app_container_sid: *mut c_void, capabilities: *mut c_void, capability_count: Dword, reserved: Dword }
#[repr(C)]
struct StartupInfo { cb: Dword, reserved: *mut u16, desktop: *mut u16, title: *mut u16, x: Dword, y: Dword, x_size: Dword, y_size: Dword, x_count_chars: Dword, y_count_chars: Dword, fill_attribute: Dword, flags: Dword, show_window: u16, reserved2_size: u16, reserved2: *mut u8, std_input: Handle, std_output: Handle, std_error: Handle }
#[repr(C)]
struct StartupInfoEx { startup: StartupInfo, attribute_list: *mut c_void }
#[repr(C)]
struct ProcessInformation { process: Handle, thread: Handle, process_id: Dword, thread_id: Dword }
#[repr(C)]
struct IoCounters { read_operations: u64, write_operations: u64, other_operations: u64, read_transfer_count: u64, write_transfer_count: u64, other_transfer_count: u64 }
#[repr(C)]
struct BasicLimit { per_process_user_time_limit: i64, per_job_user_time_limit: i64, limit_flags: Dword, minimum_working_set_size: usize, maximum_working_set_size: usize, active_process_limit: Dword, affinity: usize, priority_class: Dword, scheduling_class: Dword }
#[repr(C)]
struct ExtendedLimit { basic: BasicLimit, io: IoCounters, process_memory_limit: usize, job_memory_limit: usize, peak_process_memory_used: usize, peak_job_memory_used: usize }

#[link(name = "userenv")]
unsafe extern "system" {
    fn CreateAppContainerProfile(name: *const u16, display: *const u16, description: *const u16, capabilities: *const c_void, count: Dword, sid: *mut *mut c_void) -> i32;
    fn DeleteAppContainerProfile(name: *const u16) -> i32;
    fn GetAppContainerFolderPath(sid: *const u16, path: *mut *mut u16) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn CloseHandle(handle: Handle) -> Bool;
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: Dword, flags: Dword, size: *mut usize) -> Bool;
    fn UpdateProcThreadAttribute(list: *mut c_void, flags: Dword, attribute: usize, value: *mut c_void, size: usize, previous: *mut c_void, return_size: *mut usize) -> Bool;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(app: *const u16, command: *mut u16, process_attributes: *mut c_void, thread_attributes: *mut c_void, inherit_handles: Bool, flags: Dword, environment: *mut c_void, directory: *const u16, startup: *mut StartupInfo, info: *mut ProcessInformation) -> Bool;
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, length: Dword) -> Bool;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut Bool) -> Bool;
    fn ResumeThread(thread: Handle) -> Dword;
    fn TerminateProcess(process: Handle, code: Dword) -> Bool;
    fn WaitForSingleObject(handle: Handle, milliseconds: Dword) -> Dword;
    fn GetExitCodeProcess(process: Handle, code: *mut Dword) -> Bool;
    fn OpenProcessToken(process: Handle, access: Dword, token: *mut Handle) -> Bool;
    fn GetTokenInformation(token: Handle, class: Dword, info: *mut c_void, length: Dword, returned: *mut Dword) -> Bool;
    fn GetCurrentProcess() -> Handle;
    fn GetCurrentProcessId() -> Dword;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> Bool;
    fn EqualSid(a: *mut c_void, b: *mut c_void) -> Bool;
}
#[link(name = "ole32")]
unsafe extern "system" { fn CoTaskMemFree(memory: *mut c_void); }

fn wide(value: &std::ffi::OsStr) -> Vec<u16> { value.encode_wide().chain([0]).collect() }
fn wide_text(value: &str) -> Vec<u16> { wide(std::ffi::OsStr::new(value)) }
fn checked(ok: bool, name: &str) -> io::Result<()> { if ok { Ok(()) } else { Err(io::Error::new(io::ErrorKind::Other, format!("{name}: {}", io::Error::last_os_error()))) } }
fn hr_checked(hr: i32, name: &str) -> io::Result<()> { if hr >= 0 { Ok(()) } else { Err(io::Error::new(io::ErrorKind::Other, format!("{name}: HRESULT 0x{:08x}", hr as u32))) } }
struct OwnedHandle(Handle);
impl Drop for OwnedHandle { fn drop(&mut self) { if !self.0.is_null() { unsafe { CloseHandle(self.0); } } } }
struct Profile { name: Vec<u16>, sid: *mut c_void }
impl Drop for Profile { fn drop(&mut self) { unsafe { LocalFree(self.sid); let hr = DeleteAppContainerProfile(self.name.as_ptr()); if hr < 0 { eprintln!("profile cleanup failed: 0x{:08x}", hr as u32); } } } }

fn token_info(token: Handle, class: Dword) -> io::Result<Vec<u8>> {
    let mut len = 0;
    unsafe { GetTokenInformation(token, class, null_mut(), 0, &mut len); }
    if len == 0 || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER) { return Err(io::Error::last_os_error()); }
    let mut bytes = vec![0u8; len as usize];
    checked(unsafe { GetTokenInformation(token, class, bytes.as_mut_ptr().cast(), len, &mut len) } != 0, "GetTokenInformation")?;
    Ok(bytes)
}

fn verify_token(process: Handle, expected_sid: *mut c_void) -> io::Result<()> {
    let mut raw = null_mut();
    checked(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw) } != 0, "OpenProcessToken")?;
    let token = OwnedHandle(raw);
    let state = token_info(token.0, TOKEN_IS_APP_CONTAINER)?;
    if state.len() < 4 || u32::from_ne_bytes(state[..4].try_into().unwrap()) != 1 { return Err(io::Error::other("worker token is not AppContainer")); }
    let capabilities = token_info(token.0, TOKEN_CAPABILITIES)?;
    if capabilities.len() < 4 || u32::from_ne_bytes(capabilities[..4].try_into().unwrap()) != 0 { return Err(io::Error::other("worker has capabilities")); }
    let sid_info = token_info(token.0, TOKEN_APP_CONTAINER_SID)?;
    if sid_info.len() < size_of::<usize>() { return Err(io::Error::other("missing AppContainer SID")); }
    let actual_sid = unsafe { (sid_info.as_ptr() as *const *mut c_void).read_unaligned() };
    if actual_sid.is_null() || unsafe { EqualSid(actual_sid, expected_sid) } == 0 { return Err(io::Error::other("wrong AppContainer SID")); }
    Ok(())
}

fn profile_folder(sid: *mut c_void) -> io::Result<PathBuf> {
    let mut raw_sid = null_mut();
    checked(unsafe { ConvertSidToStringSidW(sid, &mut raw_sid) } != 0, "ConvertSidToStringSidW")?;
    let sid_text = unsafe { std::slice::from_raw_parts(raw_sid, (0..).take_while(|&i| *raw_sid.add(i) != 0).count()) }.to_vec();
    let mut folder = null_mut();
    let result = hr_checked(unsafe { GetAppContainerFolderPath(raw_sid, &mut folder) }, "GetAppContainerFolderPath");
    unsafe { LocalFree(raw_sid.cast()); }
    result?;
    if folder.is_null() { return Err(io::Error::other("empty profile folder")); }
    let folder_text = unsafe { std::slice::from_raw_parts(folder, (0..).take_while(|&i| *folder.add(i) != 0).count()) };
    let path = PathBuf::from(String::from_utf16_lossy(folder_text));
    unsafe { CoTaskMemFree(folder.cast()); }
    if sid_text.is_empty() { return Err(io::Error::other("empty SID text")); }
    Ok(path)
}

fn launch(simulate_setup_failure: bool) -> io::Result<()> {
    let profile_name = wide_text(&format!("CortelolIsolationSpike.{}.{}", unsafe { GetCurrentProcessId() }, std::process::id()));
    let mut sid = null_mut();
    hr_checked(unsafe { CreateAppContainerProfile(profile_name.as_ptr(), wide_text("Cortelol isolation spike").as_ptr(), wide_text("Temporary local test").as_ptr(), null(), 0, &mut sid) }, "CreateAppContainerProfile")?;
    if sid.is_null() { return Err(io::Error::other("profile returned no SID")); }
    let profile = Profile { name: profile_name, sid };
    let folder = profile_folder(profile.sid)?;
    let stage = folder.join("Temp").join("CortelolIsolationSpike");
    fs::create_dir_all(&stage)?;
    let worker = stage.join("worker.exe");
    fs::copy(env::current_exe()?, &worker)?;
    let secret = env::temp_dir().join(format!("cortelol-isolation-secret-{}.txt", std::process::id()));
    fs::write(&secret, b"private-test-only")?;
    let test_result = run_worker(&worker, &stage, &secret, profile.sid, simulate_setup_failure);
    let _ = fs::remove_file(&secret);
    let _ = fs::remove_file(stage.join("marker"));
    let _ = fs::remove_file(&worker);
    let _ = fs::remove_dir(&stage);
    drop(profile);
    if folder.exists() { return Err(io::Error::other("AppContainer profile folder survived cleanup")); }
    println!("PASS: temporary AppContainer profile removed");
    test_result
}

fn run_worker(worker: &Path, stage: &Path, secret: &Path, sid: *mut c_void, simulate_setup_failure: bool) -> io::Result<()> {
    let tcp = TcpListener::bind("127.0.0.1:0")?;
    let udp = UdpSocket::bind("127.0.0.1:0")?;
    let ipv6 = TcpListener::bind("[::1]:0")?;
    tcp.set_nonblocking(true)?;
    ipv6.set_nonblocking(true)?;
    udp.set_read_timeout(Some(Duration::from_millis(800)))?;
    let job = OwnedHandle(unsafe { CreateJobObjectW(null_mut(), null()) });
    if job.0.is_null() { return Err(io::Error::last_os_error()); }
    let mut limits: ExtendedLimit = unsafe { zeroed() };
    limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.basic.active_process_limit = 4;
    limits.process_memory_limit = 256 * 1024 * 1024;
    checked(unsafe { SetInformationJobObject(job.0, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION, (&limits as *const ExtendedLimit).cast(), size_of::<ExtendedLimit>() as Dword) } != 0, "SetInformationJobObject")?;
    let mut attrs_len = 0usize;
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attrs_len); }
    if attrs_len == 0 { return Err(io::Error::other("attribute list size unavailable")); }
    let mut attrs = vec![0usize; attrs_len.div_ceil(size_of::<usize>())];
    let attrs_ptr = attrs.as_mut_ptr().cast();
    checked(unsafe { InitializeProcThreadAttributeList(attrs_ptr, 1, 0, &mut attrs_len) } != 0, "InitializeProcThreadAttributeList")?;
    let mut capabilities = SecurityCapabilities { app_container_sid: sid, capabilities: null_mut(), capability_count: 0, reserved: 0 };
    let setup = checked(unsafe { UpdateProcThreadAttribute(attrs_ptr, 0, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, (&mut capabilities as *mut SecurityCapabilities).cast(), size_of::<SecurityCapabilities>(), null_mut(), null_mut()) } != 0, "UpdateProcThreadAttribute");
    if let Err(e) = setup { unsafe { DeleteProcThreadAttributeList(attrs_ptr); } return Err(e); }
    let mut startup: StartupInfoEx = unsafe { zeroed() };
    startup.startup.cb = size_of::<StartupInfoEx>() as Dword;
    startup.attribute_list = attrs_ptr;
    let app = wide(worker.as_os_str());
    let mut command = wide_text(&format!("\"{}\" --worker \"{}\" \"{}\" {} {} {}", worker.display(), stage.display(), secret.display(), tcp.local_addr()?.port(), udp.local_addr()?.port(), ipv6.local_addr()?.port()));
    let local_app_data = env::var("LOCALAPPDATA").map_err(|_| io::Error::other("LOCALAPPDATA unavailable"))?;
    let mut env_block: Vec<u16> = format!("LOCALAPPDATA={}\0SystemRoot={}\0TEMP={}\0TMP={}\0\0", local_app_data, env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()), stage.display(), stage.display()).encode_utf16().collect();
    let directory = wide(stage.as_os_str());
    let mut info: ProcessInformation = unsafe { zeroed() };
    let created = unsafe { CreateProcessW(app.as_ptr(), command.as_mut_ptr(), null_mut(), null_mut(), 0, CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT, env_block.as_mut_ptr().cast(), directory.as_ptr(), &mut startup.startup, &mut info) };
    unsafe { DeleteProcThreadAttributeList(attrs_ptr); }
    checked(created != 0, "CreateProcessW")?;
    let process = OwnedHandle(info.process);
    let thread = OwnedHandle(info.thread);
    let prepared = (|| {
        checked(unsafe { AssignProcessToJobObject(job.0, process.0) } != 0, "AssignProcessToJobObject")?;
        let mut in_job = 0;
        checked(unsafe { IsProcessInJob(process.0, job.0, &mut in_job) } != 0 && in_job != 0, "IsProcessInJob")?;
        verify_token(process.0, sid)?;
        Ok(())
    })();
    if let Err(e) = prepared { unsafe { TerminateProcess(process.0, 1); WaitForSingleObject(process.0, 5_000); } return Err(e); }
    if simulate_setup_failure {
        unsafe { TerminateProcess(process.0, 1); WaitForSingleObject(process.0, 5_000); }
        if stage.join("marker").exists() { return Err(io::Error::other("worker ran before setup completed")); }
        return Err(io::Error::other("injected setup failure refused to resume worker"));
    }
    println!("PASS: suspended worker has expected AppContainer SID, zero capabilities, and Job membership");
    if unsafe { ResumeThread(thread.0) } == u32::MAX { unsafe { TerminateProcess(process.0, 1); } return Err(io::Error::last_os_error()); }
    let wait = unsafe { WaitForSingleObject(process.0, 15_000) };
    if wait != WAIT_OBJECT_0 { unsafe { TerminateProcess(process.0, 1); } return Err(io::Error::other(format!("worker did not finish: wait={wait}"))); }
    let mut code = 0;
    checked(unsafe { GetExitCodeProcess(process.0, &mut code) } != 0, "GetExitCodeProcess")?;
    if code != 0 { return Err(io::Error::other(format!("worker failed isolation probes: bitmask 0x{code:x}"))); }
    if fs::read(stage.join("marker"))? != b"inside-profile" { return Err(io::Error::other("worker could not write its staging marker")); }
    if tcp.accept().is_ok() || ipv6.accept().is_ok() || udp.recv_from(&mut [0u8; 8]).is_ok() { return Err(io::Error::other("local listener received worker traffic")); }
    println!("PASS: worker denied direct TCP and local UDP delivery, outside-profile file read, and child breakaway");
    Ok(())
}

fn worker() -> u32 {
    let args: Vec<_> = env::args_os().collect();
    if args.len() != 7 { return 0x80; }
    let parse_port = |index: usize| args[index].to_string_lossy().parse::<u16>().ok();
    let (Some(tcp_port), Some(udp_port), Some(ipv6_port)) = (parse_port(4), parse_port(5), parse_port(6)) else { return 0x80; };
    let mut fail = 0;
    if std::net::TcpStream::connect_timeout(&format!("127.0.0.1:{tcp_port}").parse().unwrap(), Duration::from_millis(500)).is_ok() { fail |= 1; }
    if std::net::TcpStream::connect_timeout(&"1.1.1.1:443".parse().unwrap(), Duration::from_millis(500)).is_ok() { fail |= 2; }
    if std::net::TcpStream::connect_timeout(&format!("[::1]:{ipv6_port}").parse().unwrap(), Duration::from_millis(500)).is_ok() { fail |= 4; }
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") { let _ = socket.send_to(b"x", format!("127.0.0.1:{udp_port}")); }
    // DNS API success can be cached or resolved by a service, so it is not treated as proof of network access.
    let _ = std::net::ToSocketAddrs::to_socket_addrs(&("example.com", 443));
    if fs::read(&args[3]).is_ok() { fail |= 32; }
    if fs::write(Path::new(&args[2]).join("marker"), b"inside-profile").is_err() { fail |= 16; }
    let self_exe = env::current_exe().unwrap();
    match Command::new(&self_exe).arg("--child").status() {
        Ok(status) if status.success() => {}
        _ => fail |= 64,
    }
    if Command::new(self_exe).arg("--child").creation_flags(CREATE_BREAKAWAY_FROM_JOB).status().is_ok() { fail |= 128; }
    fail
}

fn child() -> u32 {
    let mut in_job = 0;
    if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &mut in_job) } == 0 || in_job == 0 { return 1; }
    let mut raw = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 { return 2; }
    let token = OwnedHandle(raw);
    match token_info(token.0, TOKEN_IS_APP_CONTAINER) {
        Ok(bytes) if bytes.len() >= 4 && u32::from_ne_bytes(bytes[..4].try_into().unwrap()) == 1 => 0,
        _ => 3,
    }
}

fn main() {
    let result = match env::args_os().nth(1).as_deref() {
        Some(value) if value == std::ffi::OsStr::new("--worker") => std::process::exit(worker() as i32),
        Some(value) if value == std::ffi::OsStr::new("--child") => std::process::exit(child() as i32),
        Some(value) if value == std::ffi::OsStr::new("--simulate-setup-failure") => launch(true),
        _ => launch(false),
    };
    if let Err(e) = result { eprintln!("FAIL CLOSED: {e}"); std::process::exit(1); }
}
