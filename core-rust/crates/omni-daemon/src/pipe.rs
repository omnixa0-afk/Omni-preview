//! Windows named-pipe IPC transport for the daemon (gated to `#[cfg(windows)]`).
//!
//! The default daemon transport is localhost TCP (dependency-free, cross-platform) — but
//! any local process that can reach 127.0.0.1 may drive the core. A named pipe replaces it
//! with OS-enforced access control: the pipe is created with a restrictive DACL granting
//! only the creating user (OWNER_RIGHTS), LocalSystem, and Administrators, and rejects
//! remote (SMB) clients. NOTE on threat coverage: this stops *other users* on a multi-user
//! machine; it does NOT (and cannot) stop another process running as the SAME user — that
//! process shares the user's token. The bigger daemon wins (UI-lifetime decoupling, headless
//! relay) are independent of the transport. See docs/decisions/daemon-ipc-phase2.md §IPC.
//!
//! Per the Android-portability rule (CONTRIBUTING.md) this calls Win32 via raw `extern "system"`
//! declarations under `#[cfg(windows)]`, with no Windows crate dependency.
//!
//! **Overlapped I/O is mandatory here**, not an optimization: the daemon must READ requests
//! and WRITE pushed events on one connection concurrently (the framed `watch` plane). A
//! *synchronous* pipe handle (and any handle duplicated from it) shares one FILE_OBJECT whose
//! I/O the kernel serializes — so a thread blocked in `ReadFile` would hold the object lock
//! and stall the event pusher's `WriteFile` forever. Creating the pipe and opening the client
//! with `FILE_FLAG_OVERLAPPED` makes the file object non-synchronous, so reads and writes run
//! independently. Each `ReadFile`/`WriteFile`/`ConnectNamedPipe` is issued overlapped and then
//! awaited with `GetOverlappedResult`, giving blocking semantics that fit the thread-per-
//! connection model without the serialization.

#![cfg(windows)]

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::raw::c_void;
use std::ptr;

use crate::IpcListener;

type Handle = *mut c_void;
const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

// CreateNamedPipe — dwOpenMode / dwPipeMode
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

// CreateFile (client)
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;

// Errors
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_NO_DATA: u32 = 232;
const ERROR_PIPE_NOT_CONNECTED: u32 = 233;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_IO_PENDING: u32 = 997;

// DuplicateHandle
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;

// SDDL revision
const SDDL_REVISION_1: u32 = 1;

/// Restrictive DACL: protected (no inherited ACEs, so Everyone is NOT added), granting
/// Generic-All only to the object owner (the creating user), LocalSystem, and Builtin
/// Administrators. This is what keeps other local users off the pipe.
const PIPE_SDDL: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)";

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut c_void,
    b_inherit_handle: i32,
}

/// Win32 `OVERLAPPED` (64-bit layout). We only set `h_event`; offsets stay zero (pipes ignore
/// them). `internal`/`internal_high` are written by the kernel.
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

impl Overlapped {
    fn with_event(event: Handle) -> Self {
        Self { internal: 0, internal_high: 0, offset: 0, offset_high: 0, h_event: event }
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        lpName: *const u16,
        dwOpenMode: u32,
        dwPipeMode: u32,
        nMaxInstances: u32,
        nOutBufferSize: u32,
        nInBufferSize: u32,
        nDefaultTimeOut: u32,
        lpSecurityAttributes: *mut SecurityAttributes,
    ) -> Handle;
    fn ConnectNamedPipe(hNamedPipe: Handle, lpOverlapped: *mut c_void) -> i32;
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: Handle,
    ) -> Handle;
    fn WaitNamedPipeW(lpNamedPipeName: *const u16, nTimeOut: u32) -> i32;
    fn ReadFile(
        hFile: Handle,
        lpBuffer: *mut u8,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;
    fn WriteFile(
        hFile: Handle,
        lpBuffer: *const u8,
        nNumberOfBytesToWrite: u32,
        lpNumberOfBytesWritten: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;
    fn GetOverlappedResult(
        hFile: Handle,
        lpOverlapped: *mut c_void,
        lpNumberOfBytesTransferred: *mut u32,
        bWait: i32,
    ) -> i32;
    fn CreateEventW(
        lpEventAttributes: *mut c_void,
        bManualReset: i32,
        bInitialState: i32,
        lpName: *const u16,
    ) -> Handle;
    fn CloseHandle(hObject: Handle) -> i32;
    fn GetLastError() -> u32;
    fn GetCurrentProcess() -> Handle;
    fn DuplicateHandle(
        hSourceProcessHandle: Handle,
        hSourceHandle: Handle,
        hTargetProcessHandle: Handle,
        lpTargetHandle: *mut Handle,
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        dwOptions: u32,
    ) -> i32;
    fn LocalFree(hMem: *mut c_void) -> *mut c_void;
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        StringSecurityDescriptor: *const u16,
        StringSDRevision: u32,
        SecurityDescriptor: *mut *mut c_void,
        SecurityDescriptorSize: *mut u32,
    ) -> i32;
}

fn last_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

/// A wide (UTF-16, NUL-terminated) copy of a string, for the `*W` Win32 calls.
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A manual-reset event for overlapped completion. The I/O manager resets it to non-signaled
/// when each operation begins, so reusing one event across sequential ops on a stream is safe.
fn create_event() -> io::Result<Handle> {
    let event = unsafe { CreateEventW(ptr::null_mut(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(last_error());
    }
    Ok(event)
}

/// Issue an overlapped op (already started) and block for its result, mapping a closed peer
/// to EOF semantics. `started` is the BOOL from ReadFile/WriteFile; `transferred` its count.
fn await_overlapped(
    handle: Handle,
    overlapped: &mut Overlapped,
    started: i32,
    transferred: u32,
) -> io::Result<u32> {
    if started != 0 {
        return Ok(transferred); // completed synchronously
    }
    let code = unsafe { GetLastError() };
    if code != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let mut done: u32 = 0;
    let ok = unsafe {
        GetOverlappedResult(handle, overlapped as *mut Overlapped as *mut c_void, &mut done, 1)
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(done)
}

/// True for the OS errors that mean "the peer closed the pipe" — surfaced as a clean EOF.
fn is_disconnect(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code as u32 == ERROR_BROKEN_PIPE
            || code as u32 == ERROR_PIPE_NOT_CONNECTED
            || code as u32 == ERROR_NO_DATA
    )
}

/// Owns the security descriptor parsed from `PIPE_SDDL`; freed with `LocalFree` on drop.
struct PipeSecurity {
    descriptor: *mut c_void,
}

impl PipeSecurity {
    fn restrictive() -> io::Result<Self> {
        let sddl = wide(PIPE_SDDL);
        let mut descriptor: *mut c_void = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(Self { descriptor })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

// SAFETY: the descriptor is an opaque, read-only kernel-allocated blob we only pass by
// pointer to CreateNamedPipeW; sharing it across threads is sound (it is never mutated).
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

/// One end of a connected named pipe, using overlapped I/O. Owns a pipe handle and a
/// completion event, both closed on drop. The read half and write half are distinct
/// `PipeStream`s over duplicated handles + separate events, so the request reader and the
/// event pusher never serialize against each other.
pub struct PipeStream {
    handle: Handle,
    event: Handle,
}

// SAFETY: a Windows HANDLE is process-global and valid from any thread; the read and write
// halves hold distinct duplicated handles and distinct events, so nothing is aliased.
unsafe impl Send for PipeStream {}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min(u32::MAX as usize) as u32;
        let mut overlapped = Overlapped::with_event(self.event);
        let mut got: u32 = 0;
        let started = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                want,
                &mut got,
                &mut overlapped as *mut Overlapped as *mut c_void,
            )
        };
        match await_overlapped(self.handle, &mut overlapped, started, got) {
            Ok(n) => Ok(n as usize),
            Err(error) if is_disconnect(&error) => Ok(0), // EOF, like a TCP FIN
            Err(error) => Err(error),
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let want = buf.len().min(u32::MAX as usize) as u32;
        let mut overlapped = Overlapped::with_event(self.event);
        let mut put: u32 = 0;
        let started = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                want,
                &mut put,
                &mut overlapped as *mut Overlapped as *mut c_void,
            )
        };
        match await_overlapped(self.handle, &mut overlapped, started, put) {
            Ok(n) => Ok(n as usize),
            Err(error) if is_disconnect(&error) => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // pipes are not user-buffered on our side
    }
}

impl Drop for PipeStream {
    fn drop(&mut self) {
        if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
        if !self.event.is_null() {
            unsafe { CloseHandle(self.event) };
        }
    }
}

/// Duplicate a handle within this process so the read and write halves own distinct handles.
fn duplicate(handle: Handle) -> io::Result<Handle> {
    let mut dup: Handle = ptr::null_mut();
    let process = unsafe { GetCurrentProcess() };
    let ok = unsafe {
        DuplicateHandle(process, handle, process, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(dup)
}

/// Split one connected (overlapped) pipe handle into boxed read/write halves: the write half
/// over a duplicated handle, each half with its own completion event. On any setup failure the
/// caller closes the original handle.
fn split(handle: Handle) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
    let read_event = create_event()?;
    let write_handle = match duplicate(handle) {
        Ok(dup) => dup,
        Err(error) => {
            unsafe { CloseHandle(read_event) };
            return Err(error);
        }
    };
    let write_event = match create_event() {
        Ok(event) => event,
        Err(error) => {
            unsafe { CloseHandle(read_event) };
            unsafe { CloseHandle(write_handle) };
            return Err(error);
        }
    };
    let reader = PipeStream { handle, event: read_event };
    let writer = PipeStream { handle: write_handle, event: write_event };
    Ok((Box::new(BufReader::new(reader)), Box::new(writer)))
}

/// A bound named-pipe endpoint the daemon accepts clients on. Each `accept` creates a fresh
/// overlapped pipe instance (with the restrictive DACL) and blocks until a client connects.
pub struct PipeListener {
    name_w: Vec<u16>,
    security: PipeSecurity,
}

impl PipeListener {
    pub fn bind(name: &str) -> io::Result<Self> {
        Ok(Self { name_w: wide(name), security: PipeSecurity::restrictive()? })
    }

    pub fn describe(&self) -> String {
        // Drop the trailing NUL for display.
        String::from_utf16_lossy(&self.name_w[..self.name_w.len().saturating_sub(1)])
    }

    fn create_instance(&self) -> io::Result<Handle> {
        let mut attributes = SecurityAttributes {
            n_length: std::mem::size_of::<SecurityAttributes>() as u32,
            lp_security_descriptor: self.security.descriptor,
            b_inherit_handle: 0,
        };
        let handle = unsafe {
            CreateNamedPipeW(
                self.name_w.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                PIPE_BUFFER_BYTES,
                PIPE_BUFFER_BYTES,
                0,
                &mut attributes,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(last_error());
        }
        Ok(handle)
    }
}

impl IpcListener for PipeListener {
    fn accept(&self) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
        let handle = self.create_instance()?;
        if let Err(error) = self.wait_for_client(handle) {
            unsafe { CloseHandle(handle) };
            return Err(error);
        }
        split(handle).inspect_err(|_| unsafe {
            CloseHandle(handle);
        })
    }
}

impl PipeListener {
    /// Block (overlapped) until a client connects to `handle`. A client that connected in the
    /// race window before `ConnectNamedPipe` shows up as `ERROR_PIPE_CONNECTED` = success.
    fn wait_for_client(&self, handle: Handle) -> io::Result<()> {
        let event = create_event()?;
        let mut overlapped = Overlapped::with_event(event);
        let started = unsafe {
            ConnectNamedPipe(handle, &mut overlapped as *mut Overlapped as *mut c_void)
        };
        let result = if started != 0 {
            Ok(())
        } else {
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => Ok(()),
                ERROR_IO_PENDING => {
                    let mut done: u32 = 0;
                    let ok = unsafe {
                        GetOverlappedResult(
                            handle,
                            &mut overlapped as *mut Overlapped as *mut c_void,
                            &mut done,
                            1,
                        )
                    };
                    if ok == 0 {
                        Err(last_error())
                    } else {
                        Ok(())
                    }
                }
                code => Err(io::Error::from_raw_os_error(code as i32)),
            }
        };
        unsafe { CloseHandle(event) };
        result
    }
}

/// Client side: connect to a named-pipe daemon and return split read/write halves (matching
/// the TCP path). Retries (up to a ~10 s deadline) while no instance is currently listening:
/// `ERROR_PIPE_BUSY` (all instances connected) waits via `WaitNamedPipeW`; `ERROR_FILE_NOT_
/// FOUND` (the daemon is still starting, or the brief window between accepts before the next
/// instance exists) backs off and retries. Any other error fails immediately.
pub fn connect(name: &str) -> io::Result<(Box<dyn BufRead + Send>, Box<dyn Write + Send>)> {
    let name_w = wide(name);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let handle = unsafe {
            CreateFileW(
                name_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return split(handle).inspect_err(|_| unsafe {
                CloseHandle(handle);
            });
        }
        let code = unsafe { GetLastError() };
        match code {
            ERROR_PIPE_BUSY => {
                // All instances are connected — wait (up to 2s) for one to free up.
                unsafe { WaitNamedPipeW(name_w.as_ptr(), 2000) };
            }
            ERROR_FILE_NOT_FOUND if std::time::Instant::now() < deadline => {
                // No instance is listening yet (daemon starting, or between accepts). Back off.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => return Err(io::Error::from_raw_os_error(code as i32)),
        }
    }
}
