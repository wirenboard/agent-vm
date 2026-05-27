// LD_PRELOAD library that intercepts execve/execvp so claude's per-session
// `--bg-spare` workers can be redirected into agent-vm.
//
// We catch the inner spawn: argv == [<claude-bin>, "--bg-spare", <claim-sock>].
// When matched, we rewrite the call to invoke our dispatcher with the original
// argv as its arguments. The dispatcher is responsible for booting a VM and
// running claude inside it.
//
// All other exec/spawn calls are passed through unchanged.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::OpenOptions;
use std::io::Write;

// --- logging --------------------------------------------------------------

fn debug_log(msg: &str) {
    // Opt-in via env var so we don't slow down or fill disk in steady state.
    let path = match std::env::var("CLAUDE_VM_SHIM_LOG") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let pid = unsafe { getpid() };
        let _ = writeln!(f, "[pid {}] {}", pid, msg);
    }
}

// --- libc bindings --------------------------------------------------------

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn getpid() -> c_int;
}

const RTLD_NEXT: *mut c_void = -1isize as *mut c_void;

unsafe fn next_sym(name: &str) -> *mut c_void {
    let cname = CString::new(name).expect("symbol name has no nul");
    dlsym(RTLD_NEXT, cname.as_ptr())
}

// --- argv helpers ---------------------------------------------------------

unsafe fn collect_argv(argv: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if argv.is_null() {
        return out;
    }
    let mut i = 0isize;
    loop {
        let p = *argv.offset(i);
        if p.is_null() {
            break;
        }
        out.push(CStr::from_ptr(p).to_owned());
        i += 1;
        // Defensive cap to avoid wandering off bad input.
        if i > 4096 {
            break;
        }
    }
    out
}

unsafe fn collect_envp(envp: *const *const c_char) -> Vec<CString> {
    let mut out = Vec::new();
    if envp.is_null() {
        return out;
    }
    let mut i = 0isize;
    loop {
        let p = *envp.offset(i);
        if p.is_null() {
            break;
        }
        out.push(CStr::from_ptr(p).to_owned());
        i += 1;
        if i > 16384 {
            break;
        }
    }
    out
}

// Borrowed pointer vector — must outlive the exec call.
struct CArgv {
    _owned: Vec<CString>,
    ptrs: Vec<*const c_char>,
}

impl CArgv {
    fn new(items: Vec<CString>) -> Self {
        let mut ptrs: Vec<*const c_char> = items.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        Self { _owned: items, ptrs }
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.ptrs.as_ptr()
    }
}

// --- interception decision -------------------------------------------------

/// Returns Some((dispatcher_path, new_argv)) if this exec call should be
/// redirected through the dispatcher. The new_argv is: [dispatcher, original_path, original_argv[1..]].
fn maybe_redirect(path_c: &CStr, argv: &[CString]) -> Option<(CString, Vec<CString>)> {
    if argv.len() < 2 {
        return None;
    }
    let arg1 = argv[1].to_string_lossy();

    // Match the per-session spawn from `claude remote-control`:
    //   `claude --print --sdk-url <url> --session-id <id> …`
    //
    // `--print` alone is the same flag ordinary `claude --print "hi"` uses,
    // so we also require `--sdk-url` to be present to discriminate.
    if arg1 != "--print" || !argv.iter().any(|a| a.to_bytes() == b"--sdk-url") {
        return None;
    }

    // Dispatcher path is configurable.
    let dispatcher = match std::env::var("CLAUDE_VM_SHIM_DISPATCHER") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            debug_log("CLAUDE_VM_SHIM_DISPATCHER not set; passing through");
            return None;
        }
    };
    debug_log(&format!(
        "intercept: --print --sdk-url (path={:?})",
        path_c.to_string_lossy()
    ));
    let dispatcher_c = match CString::new(dispatcher.clone()) {
        Ok(c) => c,
        Err(_) => return None,
    };

    // Build new argv: [dispatcher, original_path, original_argv[1..]]
    let mut new_argv = Vec::with_capacity(argv.len() + 1);
    new_argv.push(dispatcher_c.clone());
    new_argv.push(path_c.to_owned());
    for a in argv.iter().skip(1) {
        new_argv.push(a.clone());
    }

    debug_log(&format!(
        "redirecting to dispatcher {:?} (orig path {:?})",
        dispatcher,
        path_c.to_string_lossy()
    ));

    Some((dispatcher_c, new_argv))
}

// --- intercepted entry points ---------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if path.is_null() {
        // Let libc handle the error path.
        let sym = next_sym("execve");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(path, argv, envp);
    }

    let path_c = CStr::from_ptr(path);
    let argv_v = collect_argv(argv);

    if let Some((new_path, new_argv)) = maybe_redirect(path_c, &argv_v) {
        let envp_v = collect_envp(envp);
        let envp_ptrs = CArgv::new(envp_v);
        let argv_ptrs = CArgv::new(new_argv);

        let sym = next_sym("execve");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(new_path.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }

    let sym = next_sym("execve");
    if sym.is_null() {
        return -1;
    }
    let f: extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int =
        std::mem::transmute(sym);
    f(path, argv, envp)
}

#[no_mangle]
pub unsafe extern "C" fn execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    if path.is_null() {
        let sym = next_sym("execv");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(path, argv);
    }

    let path_c = CStr::from_ptr(path);
    let argv_v = collect_argv(argv);

    if let Some((new_path, new_argv)) = maybe_redirect(path_c, &argv_v) {
        let argv_ptrs = CArgv::new(new_argv);
        let sym = next_sym("execv");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(new_path.as_ptr(), argv_ptrs.as_ptr());
    }

    let sym = next_sym("execv");
    if sym.is_null() {
        return -1;
    }
    let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int = std::mem::transmute(sym);
    f(path, argv)
}

#[no_mangle]
pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    // execvp may resolve `file` via PATH; we still match on argv content.
    if file.is_null() {
        let sym = next_sym("execvp");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(file, argv);
    }

    let file_c = CStr::from_ptr(file);
    let argv_v = collect_argv(argv);

    if let Some((new_path, new_argv)) = maybe_redirect(file_c, &argv_v) {
        let argv_ptrs = CArgv::new(new_argv);
        // For the dispatcher, use execv (full path) regardless of what came in,
        // since we're passing an absolute path.
        let sym = next_sym("execv");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int =
            std::mem::transmute(sym);
        return f(new_path.as_ptr(), argv_ptrs.as_ptr());
    }

    let sym = next_sym("execvp");
    if sym.is_null() {
        return -1;
    }
    let f: extern "C" fn(*const c_char, *const *const c_char) -> c_int = std::mem::transmute(sym);
    f(file, argv)
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawn(
    pid: *mut c_int,
    path: *const c_char,
    file_actions: *const c_void,
    attrp: *const c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if path.is_null() {
        let sym = next_sym("posix_spawn");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(
            *mut c_int,
            *const c_char,
            *const c_void,
            *const c_void,
            *const *const c_char,
            *const *const c_char,
        ) -> c_int = std::mem::transmute(sym);
        return f(pid, path, file_actions, attrp, argv, envp);
    }

    let path_c = CStr::from_ptr(path);
    let argv_v = collect_argv(argv);

    if let Some((new_path, new_argv)) = maybe_redirect(path_c, &argv_v) {
        let envp_v = collect_envp(envp);
        let envp_ptrs = CArgv::new(envp_v);
        let argv_ptrs = CArgv::new(new_argv);

        let sym = next_sym("posix_spawn");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(
            *mut c_int,
            *const c_char,
            *const c_void,
            *const c_void,
            *const *const c_char,
            *const *const c_char,
        ) -> c_int = std::mem::transmute(sym);
        return f(
            pid,
            new_path.as_ptr(),
            file_actions,
            attrp,
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
        );
    }

    let sym = next_sym("posix_spawn");
    if sym.is_null() {
        return -1;
    }
    let f: extern "C" fn(
        *mut c_int,
        *const c_char,
        *const c_void,
        *const c_void,
        *const *const c_char,
        *const *const c_char,
    ) -> c_int = std::mem::transmute(sym);
    f(pid, path, file_actions, attrp, argv, envp)
}

#[no_mangle]
pub unsafe extern "C" fn posix_spawnp(
    pid: *mut c_int,
    file: *const c_char,
    file_actions: *const c_void,
    attrp: *const c_void,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    if file.is_null() {
        let sym = next_sym("posix_spawnp");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(
            *mut c_int,
            *const c_char,
            *const c_void,
            *const c_void,
            *const *const c_char,
            *const *const c_char,
        ) -> c_int = std::mem::transmute(sym);
        return f(pid, file, file_actions, attrp, argv, envp);
    }

    let file_c = CStr::from_ptr(file);
    let argv_v = collect_argv(argv);

    if let Some((new_path, new_argv)) = maybe_redirect(file_c, &argv_v) {
        let envp_v = collect_envp(envp);
        let envp_ptrs = CArgv::new(envp_v);
        let argv_ptrs = CArgv::new(new_argv);

        let sym = next_sym("posix_spawn");
        if sym.is_null() {
            return -1;
        }
        let f: extern "C" fn(
            *mut c_int,
            *const c_char,
            *const c_void,
            *const c_void,
            *const *const c_char,
            *const *const c_char,
        ) -> c_int = std::mem::transmute(sym);
        return f(
            pid,
            new_path.as_ptr(),
            file_actions,
            attrp,
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
        );
    }

    let sym = next_sym("posix_spawnp");
    if sym.is_null() {
        return -1;
    }
    let f: extern "C" fn(
        *mut c_int,
        *const c_char,
        *const c_void,
        *const c_void,
        *const *const c_char,
        *const *const c_char,
    ) -> c_int = std::mem::transmute(sym);
    f(pid, file, file_actions, attrp, argv, envp)
}
