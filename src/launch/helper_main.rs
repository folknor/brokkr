//! `brokkr-launch`: the last hop between cargo, or brokkr, and a program.
//!
//! It must have no ELF interpreter and no shared-library dependency. A
//! dynamically linked helper would run inside the target's launch environment,
//! where an `LD_PRELOAD` from cargo `[env]` can interpose `execve` and launch
//! the program with a changed environment after every check here passed. So
//! this file is `no_std` and `no_main`: raw Linux syscalls, no libc, no
//! allocator. It is not a cargo target. `build.rs` compiles it on its own
//! with rustc, `-C panic=abort` and a static link with no start files, since
//! a `no_std` executable cannot link under the package's unwinding profiles.
//! brokkr embeds the result and writes it out on first use
//! (`crate::launch::helper`). The contract is `docs/commands/run.md` ("How a
//! program is launched").
//!
//! Three modes, chosen by argv and by the name it was executed under:
//!
//! - `brokkr-launch --plan PLAN EXE ARGS...`: cargo's runner for a launch cargo
//!   performs, with cargo's launch environment. First creates the receipt
//!   `PLAN.receipt` holding `started`, before anything can fail, so a missing
//!   receipt means cargo never ran the helper. Then applies the plan's `set`
//!   entries over the inherited environment and, when the plan names a `cwd`,
//!   makes `EXE` absolute against the starting directory and changes to it.
//!   Execs the plan's runner with `EXE ARGS...` as its arguments, or `EXE`
//!   itself with `argv[0]` as cargo gave it. A failure after the receipt adds
//!   `failed` to it.
//! - `brokkr-launch --probe OUT EXE ARGS...`: cargo's runner for a probe.
//!   Writes the working directory, the executable path and the complete
//!   environment cargo gave it to `OUT`, then exits 0 without running
//!   anything.
//! - any other name (a copy placed as a `--with` peer's `BROKKR_WITH_EXE`):
//!   reads `SELF.brokkr-plan` beside its own path and execs the plan's `exe`
//!   with exactly the plan's environment plus the live values of its `keep`
//!   keys, passing its own arguments through.
//!
//! Plan format: the magic `brokkr-launch-plan-v1`, then `TAG VALUE` pairs, every
//! token NUL-terminated. Tags: `cwd DIR`, `env inherit|replace`, `set
//! KEY=VALUE` (each key at most once), `keep KEY`, `runner ARG` (repeatable, in
//! order) and `exe PATH`. brokkr writes it (`crate::launch`).
//!
//! A runner program with no `/` is searched on the `PATH` of the environment
//! being launched, relative entries against the starting directory, as
//! cargo's own spawn of it would. Every failure exits 127 after one
//! `brokkr-launch:` line on stderr.

#![no_std]
#![no_main]

mod helper {
    use core::arch::{asm, naked_asm};

    // -----------------------------------------------------------------------
    // Syscalls
    // -----------------------------------------------------------------------

    #[cfg(target_arch = "x86_64")]
    mod nr {
        pub const READ: usize = 0;
        pub const WRITE: usize = 1;
        pub const CLOSE: usize = 3;
        pub const EXECVE: usize = 59;
        pub const GETCWD: usize = 79;
        pub const CHDIR: usize = 80;
        pub const EXIT_GROUP: usize = 231;
        pub const OPENAT: usize = 257;
        pub const READLINKAT: usize = 267;
    }

    #[cfg(target_arch = "aarch64")]
    mod nr {
        pub const GETCWD: usize = 17;
        pub const CHDIR: usize = 49;
        pub const OPENAT: usize = 56;
        pub const CLOSE: usize = 57;
        pub const READ: usize = 63;
        pub const WRITE: usize = 64;
        pub const READLINKAT: usize = 78;
        pub const EXIT_GROUP: usize = 94;
        pub const EXECVE: usize = 221;
    }

    /// `AT_FDCWD` (-100) as the register value the kernel reads.
    const AT_FDCWD: usize = (-100_isize).cast_unsigned();
    const O_RDONLY: usize = 0;
    const O_WRONLY: usize = 1;
    const O_CREAT: usize = 0o100;
    const O_EXCL: usize = 0o200;
    const O_CLOEXEC: usize = 0o2_000_000;
    const EINTR: isize = -4;
    const ENOENT: isize = -2;
    const EACCES: isize = -13;
    const ENOTDIR: isize = -20;

    #[cfg(target_arch = "x86_64")]
    unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
        let ret: isize;
        // SAFETY: the caller passes a valid syscall and arguments.
        unsafe {
            asm!("syscall", inlateout("rax") n.cast_signed() => ret, in("rdi") a, in("rsi") b,
                 in("rdx") c, lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        ret
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn syscall4(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
        let ret: isize;
        // SAFETY: the caller passes a valid syscall and arguments.
        unsafe {
            asm!("syscall", inlateout("rax") n.cast_signed() => ret, in("rdi") a, in("rsi") b,
                 in("rdx") c, in("r10") d, lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        ret
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
        let ret: isize;
        // SAFETY: the caller passes a valid syscall and arguments.
        unsafe {
            asm!("svc 0", in("x8") n, inlateout("x0") a.cast_signed() => ret, in("x1") b,
                 in("x2") c, options(nostack));
        }
        ret
    }

    #[cfg(target_arch = "aarch64")]
    unsafe fn syscall4(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
        let ret: isize;
        // SAFETY: the caller passes a valid syscall and arguments.
        unsafe {
            asm!("svc 0", in("x8") n, inlateout("x0") a.cast_signed() => ret, in("x1") b,
                 in("x2") c, in("x3") d, options(nostack));
        }
        ret
    }

    fn exit(code: usize) -> ! {
        loop {
            // SAFETY: exit_group takes one integer and does not return.
            unsafe { syscall3(nr::EXIT_GROUP, code, 0, 0) };
        }
    }

    fn write_all(fd: usize, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            // SAFETY: the pointer and length describe a live slice.
            let n = unsafe { syscall3(nr::WRITE, fd, bytes.as_ptr() as usize, bytes.len()) };
            if n == EINTR {
                continue;
            }
            match usize::try_from(n) {
                Ok(0) | Err(_) => return false,
                Ok(n) => bytes = bytes.get(n..).unwrap_or(&[]),
            }
        }
        true
    }

    /// A NUL-terminated string's bytes, without the NUL.
    ///
    /// # Safety
    /// `p` points at a NUL-terminated string that outlives the result.
    unsafe fn cstr<'a>(p: *const u8) -> &'a [u8] {
        let mut len = 0;
        // SAFETY: the caller promises termination within the allocation. The
        // read is volatile so the loop is not turned into a `strlen` call,
        // which nothing here provides.
        while unsafe { p.add(len).read_volatile() } != 0 {
            len += 1;
        }
        // SAFETY: `len` bytes from `p` were just read.
        unsafe { core::slice::from_raw_parts(p, len) }
    }

    fn decimal(mut n: usize, buf: &mut [u8; 20]) -> &[u8] {
        let mut i = buf.len();
        loop {
            i -= 1;
            buf[i] = b'0' + u8::try_from(n % 10).unwrap_or(0);
            n /= 10;
            if n == 0 || i == 0 {
                break;
            }
        }
        &buf[i..]
    }

    /// The receipt's descriptor once created, so a failure can be recorded.
    static mut RECEIPT: Option<usize> = None;

    /// One `brokkr-launch: WHAT[: DETAIL][ (errno N)]` line on stderr, a
    /// `failed` line in the receipt if there is one, then 127.
    fn die(what: &[u8], detail: &[u8], errno: isize) -> ! {
        // SAFETY: single-threaded; read once on the way out.
        if let Some(fd) = unsafe { *(&raw const RECEIPT) } {
            write_all(fd, b"failed\n");
        }
        write_all(2, b"brokkr-launch: ");
        write_all(2, what);
        if !detail.is_empty() {
            write_all(2, b": ");
            write_all(2, detail);
        }
        if errno < 0 {
            let mut buf = [0; 20];
            write_all(2, b" (errno ");
            write_all(2, decimal(errno.unsigned_abs(), &mut buf));
            write_all(2, b")");
        }
        write_all(2, b"\n");
        exit(127)
    }

    // -----------------------------------------------------------------------
    // Fixed storage: no allocator, so every buffer is static and bounded. The
    // pointer arrays are sized past what a Linux exec accepts (its strings
    // share a budget of a quarter of the stack limit, and each costs at least
    // two bytes), and untouched pages of bss cost nothing.
    // -----------------------------------------------------------------------

    const PLAN_MAX: usize = 1 << 22;
    const PATH_MAX: usize = 4096;
    /// Pointers in argv or envp.
    const VEC_MAX: usize = 1 << 21;
    /// Entries of one plan tag: each costs at least four bytes of plan.
    const ENTRY_MAX: usize = PLAN_MAX / 4;

    struct Storage {
        plan: [u8; PLAN_MAX],
        exe: [u8; PATH_MAX * 2],
        candidate: [u8; PATH_MAX * 3],
        plan_path: [u8; PATH_MAX + 16],
        start_cwd: [u8; PATH_MAX],
        envp: [*const u8; VEC_MAX + 1],
        argv: [*const u8; VEC_MAX + 1],
        sets: [*const u8; ENTRY_MAX],
        keeps: [*const u8; ENTRY_MAX],
        runner: [*const u8; ENTRY_MAX],
    }

    static mut STORAGE: Storage = Storage {
        plan: [0; PLAN_MAX],
        exe: [0; PATH_MAX * 2],
        candidate: [0; PATH_MAX * 3],
        plan_path: [0; PATH_MAX + 16],
        start_cwd: [0; PATH_MAX],
        envp: [core::ptr::null(); VEC_MAX + 1],
        argv: [core::ptr::null(); VEC_MAX + 1],
        sets: [core::ptr::null(); ENTRY_MAX],
        keeps: [core::ptr::null(); ENTRY_MAX],
        runner: [core::ptr::null(); ENTRY_MAX],
    };

    /// One field of [`STORAGE`], borrowed alone, so borrows of two fields
    /// never alias. Each field has one user at a time: the process is
    /// single-threaded and no borrow of a field outlives the next one taken.
    macro_rules! field {
        ($name:ident) => {
            // SAFETY: see the macro's doc; the field pointer is valid for the
            // program's lifetime.
            unsafe { &mut *(&raw mut STORAGE.$name) }
        };
    }

    // -----------------------------------------------------------------------
    // The plan
    // -----------------------------------------------------------------------

    #[derive(PartialEq, Eq, Clone, Copy)]
    enum EnvMode {
        Inherit,
        Replace,
    }

    /// The parsed plan. Values point into the plan buffer, whose tokens are
    /// NUL-terminated, so each is already a C string; the entry lists live in
    /// [`Storage`].
    struct Plan {
        cwd: Option<*const u8>,
        exe: Option<*const u8>,
        env: EnvMode,
        sets: usize,
        keeps: usize,
        runner: usize,
    }

    const MAGIC: &[u8] = b"brokkr-launch-plan-v1";

    fn read_file(path: *const u8, buf: &mut [u8]) -> usize {
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe { syscall4(nr::OPENAT, AT_FDCWD, path as usize, O_RDONLY | O_CLOEXEC, 0) };
        let Ok(fd) = usize::try_from(fd) else {
            // SAFETY: as above.
            die(b"cannot open the plan", unsafe { cstr(path) }, fd);
        };
        let mut len = 0;
        loop {
            if len == buf.len() {
                die(b"the plan is too large", b"", 0);
            }
            let room = &mut buf[len..];
            // SAFETY: `room` is a live, writable slice.
            let n = unsafe { syscall3(nr::READ, fd, room.as_mut_ptr() as usize, room.len()) };
            if n == EINTR {
                continue;
            }
            match usize::try_from(n) {
                Ok(0) => break,
                Ok(n) => len += n,
                Err(_) => die(b"cannot read the plan", b"", n),
            }
        }
        // SAFETY: `fd` is open.
        unsafe { syscall3(nr::CLOSE, fd, 0, 0) };
        len
    }

    /// The key of `KEY=VALUE`, or the whole entry when it has no `=`.
    fn key_of(entry: &[u8]) -> &[u8] {
        match entry.iter().position(|&b| b == b'=') {
            Some(i) => &entry[..i],
            None => entry,
        }
    }

    fn push(list: &mut [*const u8], count: &mut usize, ptr: *const u8) {
        if *count == list.len() {
            die(b"the plan has too many entries", b"", 0);
        }
        list[*count] = ptr;
        *count += 1;
    }

    /// Parse strictly: the magic, then whole `TAG VALUE` pairs, every token
    /// NUL-terminated, no tag empty or unknown, each `set` key once.
    fn parse(buf: &[u8]) -> Plan {
        let mut plan = Plan {
            cwd: None,
            exe: None,
            env: EnvMode::Inherit,
            sets: 0,
            keeps: 0,
            runner: 0,
        };
        let Some(body) = buf.strip_suffix(&[0]) else {
            die(b"the plan is truncated", b"", 0);
        };
        let mut tokens = body.split(|&b| b == 0);
        if tokens.next() != Some(MAGIC) {
            die(b"not a brokkr-launch plan", b"", 0);
        }
        while let Some(tag) = tokens.next() {
            let Some(value) = tokens.next() else {
                die(b"a plan entry has no value", tag, 0);
            };
            let ptr = value.as_ptr();
            match tag {
                b"cwd" => plan.cwd = Some(ptr),
                b"exe" => plan.exe = Some(ptr),
                b"env" => {
                    plan.env = match value {
                        b"inherit" => EnvMode::Inherit,
                        b"replace" => EnvMode::Replace,
                        _ => die(b"unknown env mode", value, 0),
                    };
                }
                b"set" => {
                    if !value.contains(&b'=') {
                        die(b"a set entry has no `=`", value, 0);
                    }
                    let key = key_of(value);
                    let dup = field!(sets)[..plan.sets]
                        .iter()
                        // SAFETY: earlier values are NUL-terminated in the buffer.
                        .any(|&prev| key_of(unsafe { cstr(prev) }) == key);
                    if dup {
                        die(b"the plan sets a variable twice", key, 0);
                    }
                    push(field!(sets), &mut plan.sets, ptr);
                }
                b"keep" => push(field!(keeps), &mut plan.keeps, ptr),
                b"runner" => push(field!(runner), &mut plan.runner, ptr),
                _ => die(b"unknown plan entry", tag, 0),
            }
        }
        plan
    }

    // -----------------------------------------------------------------------
    // Modes
    // -----------------------------------------------------------------------

    fn getcwd(buf: &mut [u8]) -> &[u8] {
        // SAFETY: the buffer is live and writable for its length.
        let n = unsafe { syscall3(nr::GETCWD, buf.as_mut_ptr() as usize, buf.len(), 0) };
        if n < 0 {
            die(b"cannot read the working directory", b"", n);
        }
        // SAFETY: getcwd wrote a NUL-terminated path.
        unsafe { cstr(buf.as_ptr()) }
    }

    fn create(path: *const u8, what: &[u8]) -> usize {
        let flags = O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC;
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe { syscall4(nr::OPENAT, AT_FDCWD, path as usize, flags, 0o600) };
        match usize::try_from(fd) {
            Ok(fd) => fd,
            // SAFETY: as above.
            Err(_) => die(what, unsafe { cstr(path) }, fd),
        }
    }

    /// `--probe OUT EXE ARGS...`: record what cargo handed over, run nothing.
    fn probe(out: *const u8, exe: *const u8, envp: *const *const u8) -> ! {
        let fd = create(out, b"cannot create the probe record");
        let cwd = getcwd(field!(start_cwd));
        let mut ok = write_all(fd, b"brokkr-launch-probe-v1\0cwd\0")
            && write_all(fd, cwd)
            && write_all(fd, b"\0exe\0")
            // SAFETY: argv strings are NUL-terminated.
            && write_all(fd, unsafe { cstr(exe) })
            && write_all(fd, b"\0");
        let mut i = 0;
        loop {
            // SAFETY: envp is a NULL-terminated array of C strings.
            let entry = unsafe { *envp.add(i) };
            if entry.is_null() || !ok {
                break;
            }
            ok = write_all(fd, b"env\0")
                // SAFETY: as above.
                && write_all(fd, unsafe { cstr(entry) })
                && write_all(fd, b"\0");
            i += 1;
        }
        if !ok {
            die(b"cannot write the probe record", b"", 0);
        }
        // SAFETY: `fd` is open.
        unsafe { syscall3(nr::CLOSE, fd, 0, 0) };
        exit(0)
    }

    /// `dir/name` (a relative `dir` taken against `base`), NUL-terminated, in
    /// `out`; `None` when it does not fit.
    fn join(out: &mut [u8], base: &[u8], dir: &[u8], name: &[u8]) -> Option<*const u8> {
        let mut len = 0;
        let mut put = |bytes: &[u8]| -> Option<()> {
            let end = len + bytes.len();
            out.get_mut(len..end)?.copy_from_slice(bytes);
            len = end;
            Some(())
        };
        if dir.first() != Some(&b'/') && !base.is_empty() {
            put(base)?;
            put(b"/")?;
        }
        put(dir)?;
        if !dir.is_empty() && dir.last() != Some(&b'/') {
            put(b"/")?;
        }
        put(name)?;
        put(&[0])?;
        Some(out.as_ptr())
    }

    /// The value of `KEY` in a NULL-terminated envp, if present.
    fn env_value(envp: *const *const u8, key: &[u8]) -> Option<&'static [u8]> {
        let mut i = 0;
        loop {
            // SAFETY: envp is a NULL-terminated array of C strings.
            let entry = unsafe { *envp.add(i) };
            if entry.is_null() {
                return None;
            }
            // SAFETY: as above.
            let bytes = unsafe { cstr(entry) };
            if key_of(bytes) == key && bytes.len() > key.len() {
                return bytes.get(key.len() + 1..);
            }
            i += 1;
        }
    }

    /// The environment of the exec: under `inherit`, every live entry the plan
    /// does not set, then the plan's sets; under `replace`, the live entries
    /// of the plan's `keep` keys, then the plan's sets.
    fn compose_env(plan: &Plan, live: *const *const u8) {
        let mut n = 0;
        let mut i = 0;
        loop {
            // SAFETY: envp is a NULL-terminated array of C strings.
            let entry = unsafe { *live.add(i) };
            if entry.is_null() {
                break;
            }
            i += 1;
            // SAFETY: as above.
            let key = key_of(unsafe { cstr(entry) });
            let overridden = field!(sets)[..plan.sets]
                .iter()
                // SAFETY: plan values are NUL-terminated inside the buffer.
                .any(|&set| key_of(unsafe { cstr(set) }) == key);
            let wanted = !overridden
                && match plan.env {
                    EnvMode::Inherit => true,
                    EnvMode::Replace => field!(keeps)[..plan.keeps]
                        .iter()
                        // SAFETY: as above.
                        .any(|&keep| unsafe { cstr(keep) } == key),
                };
            if wanted {
                if n == VEC_MAX {
                    die(b"too many environment entries", b"", 0);
                }
                field!(envp)[n] = entry;
                n += 1;
            }
        }
        for i in 0..plan.sets {
            if n == VEC_MAX {
                die(b"too many environment entries", b"", 0);
            }
            field!(envp)[n] = field!(sets)[i];
            n += 1;
        }
        field!(envp)[n] = core::ptr::null();
    }

    /// Exec `path` with the composed argv and envp, returning the error if it
    /// does not replace this process. A file the kernel will not execute as
    /// a binary (`ENOEXEC`: a script with no `#!` line) is run with
    /// `/bin/sh`, as execvp does, and as cargo's own launch therefore does:
    /// argv becomes `/bin/sh PATH ARGS...`.
    fn exec_file(path: *const u8) -> isize {
        const ENOEXEC: isize = -8;
        let argv = field!(argv);
        let envp = field!(envp).as_ptr() as usize;
        // SAFETY: argv and envp are NULL-terminated arrays of C strings.
        let r = unsafe { syscall3(nr::EXECVE, path as usize, argv.as_ptr() as usize, envp) };
        if r != ENOEXEC {
            return r;
        }
        let len = argv.iter().position(|p| p.is_null()).unwrap_or(argv.len());
        if len + 1 >= argv.len() {
            return r;
        }
        // Shift ARGS one right over the old argv[0] and the terminator.
        argv.copy_within(1..len + 1, 2);
        let shell = b"/bin/sh\0";
        argv[0] = shell.as_ptr();
        argv[1] = path;
        // SAFETY: as above; the shifted array keeps its NULL terminator.
        let r = unsafe { syscall3(nr::EXECVE, shell.as_ptr() as usize, argv.as_ptr() as usize, envp) };
        // argv is shifted now, so no other candidate may be tried with it.
        // SAFETY: `path` is NUL-terminated.
        die(b"cannot execute with /bin/sh", unsafe { cstr(path) }, r)
    }

    /// Change directory if the plan says so, then exec `program` with the
    /// composed argv and envp: directly when it names a path, else searched
    /// on the composed `PATH` as execvp does, a relative entry taken against
    /// the starting directory.
    fn exec(plan: &Plan, program: *const u8, start_cwd: &[u8]) -> ! {
        if let Some(dir) = plan.cwd {
            // SAFETY: plan values are NUL-terminated.
            let r = unsafe { syscall3(nr::CHDIR, dir as usize, 0, 0) };
            if r < 0 {
                // SAFETY: as above.
                die(b"cannot change directory", unsafe { cstr(dir) }, r);
            }
        }
        // SAFETY: argv strings and plan values are NUL-terminated.
        let name = unsafe { cstr(program) };
        if name.contains(&b'/') {
            let r = exec_file(program);
            die(b"cannot execute", name, r);
        }
        // glibc's execvp default for an unset PATH, which cargo's own spawn
        // of the runner would use.
        let search = env_value(field!(envp).as_ptr(), b"PATH").unwrap_or(b"/bin:/usr/bin");
        let mut last = ENOENT;
        // Without a directory change a relative entry stays relative, as glibc
        // passes it (a script runner sees it in $0); with one, it is anchored
        // to the directory cargo started the helper in.
        let base: &[u8] = if plan.cwd.is_some() { start_cwd } else { b"" };
        for dir in search.split(|&b| b == b':') {
            let dir = if dir.is_empty() { b".".as_slice() } else { dir };
            let Some(candidate) = join(field!(candidate), base, dir, name) else {
                continue;
            };
            let r = exec_file(candidate);
            match r {
                ENOENT | ENOTDIR => {}
                EACCES => last = EACCES,
                _ => die(b"cannot execute", name, r),
            }
        }
        die(b"cannot find on PATH", name, last)
    }

    /// `--plan PLAN EXE ARGS...`.
    fn plan_mode(plan_path: *const u8, exe: *const u8, args: &[*const u8], live: *const *const u8) -> ! {
        // The receipt first, before anything else can fail.
        // SAFETY: argv strings are NUL-terminated.
        let path = unsafe { cstr(plan_path) };
        let suffix = b".receipt\0";
        let Some(slot) = field!(plan_path).get_mut(..path.len() + suffix.len()) else {
            die(b"the plan path is too long", path, 0);
        };
        slot[..path.len()].copy_from_slice(path);
        slot[path.len()..].copy_from_slice(suffix);
        let receipt = create(field!(plan_path).as_ptr(), b"cannot create the receipt");
        if !write_all(receipt, b"started\n") {
            die(b"cannot write the receipt", b"", 0);
        }
        // SAFETY: single-threaded; set once.
        unsafe { *(&raw mut RECEIPT) = Some(receipt) };

        let len = read_file(plan_path, field!(plan));
        let plan = parse(&field!(plan)[..len]);
        if plan.exe.is_some() || plan.env != EnvMode::Inherit {
            die(b"a --plan plan takes its executable from cargo and inherits", b"", 0);
        }
        let start_cwd: &[u8] = getcwd(field!(start_cwd));

        // A directory change would break a path relative to cargo's: make it
        // absolute first. Without one, everything stays exactly as cargo gave.
        // SAFETY: argv strings are NUL-terminated.
        let given = unsafe { cstr(exe) };
        let target = if plan.cwd.is_some() && given.first() != Some(&b'/') {
            match join(field!(exe), start_cwd, b"", given) {
                Some(p) => p,
                None => die(b"the executable path is too long", given, 0),
            }
        } else {
            exe
        };
        compose_env(&plan, live);
        let mut n = 0;
        let mut add = |arg: *const u8| {
            if n == VEC_MAX {
                die(b"too many arguments", b"", 0);
            }
            field!(argv)[n] = arg;
            n += 1;
        };
        let program = if plan.runner > 0 {
            for i in 0..plan.runner {
                add(field!(runner)[i]);
            }
            add(target);
            field!(runner)[0]
        } else {
            add(if plan.cwd.is_some() { target } else { exe });
            target
        };
        for &arg in args {
            add(arg);
        }
        field!(argv)[n] = core::ptr::null();
        exec(&plan, program, start_cwd)
    }

    /// Peer mode: the plan sits beside this copy, `NAME.brokkr-plan`.
    fn peer_mode(me: &[u8], args: &[*const u8], live: *const *const u8) -> ! {
        let suffix = b".brokkr-plan\0";
        let Some(slot) = field!(plan_path).get_mut(..me.len() + suffix.len()) else {
            die(b"the launcher path is too long", me, 0);
        };
        slot[..me.len()].copy_from_slice(me);
        slot[me.len()..].copy_from_slice(suffix);
        let len = read_file(field!(plan_path).as_ptr(), field!(plan));
        let plan = parse(&field!(plan)[..len]);
        let Some(exe) = plan.exe else {
            die(b"a peer plan names no executable", b"", 0);
        };
        let start_cwd: &[u8] = getcwd(field!(start_cwd));
        compose_env(&plan, live);
        let mut n = 0;
        for &arg in core::iter::once(&exe).chain(args) {
            if n == VEC_MAX {
                die(b"too many arguments", b"", 0);
            }
            field!(argv)[n] = arg;
            n += 1;
        }
        field!(argv)[n] = core::ptr::null();
        exec(&plan, exe, start_cwd)
    }

    /// The path this process was executed as, from `/proc/self/exe`, in the
    /// candidate buffer.
    fn self_path() -> &'static [u8] {
        let link = b"/proc/self/exe\0";
        let buf = field!(candidate);
        let room = buf.len();
        let out = buf.as_mut_ptr() as usize;
        // SAFETY: the link name is NUL-terminated and the buffer is writable.
        let n = unsafe { syscall4(nr::READLINKAT, AT_FDCWD, link.as_ptr() as usize, out, room) };
        match usize::try_from(n) {
            Ok(n) if n < room => &field!(candidate)[..n],
            _ => die(b"cannot read /proc/self/exe", b"", n),
        }
    }

    /// The process entry, with the initial stack pointer: `argc`, then the argv
    /// pointers and a NULL, then the envp pointers and a NULL.
    unsafe extern "C" fn entry(sp: *const usize) -> ! {
        // SAFETY: the kernel lays out the initial stack this way.
        let argc = unsafe { *sp };
        // SAFETY: as above.
        let argv = unsafe { sp.add(1) }.cast::<*const u8>();
        // SAFETY: as above.
        let envp = unsafe { argv.add(argc + 1) };
        // SAFETY: argv holds `argc` valid pointers.
        let args: &[*const u8] = unsafe { core::slice::from_raw_parts(argv, argc) };

        // SAFETY: argv strings are NUL-terminated.
        let first = args.get(1).map(|&a| unsafe { cstr(a) });
        let named = args
            .first()
            // SAFETY: as above.
            .map(|&a| unsafe { cstr(a) })
            .and_then(|a| a.rsplit(|&b| b == b'/').next())
            == Some(b"brokkr-launch".as_slice());
        // argv[0] picks the mode cheaply, /proc/self/exe confirms a peer: a
        // peer is exactly a copy whose own file name is not brokkr-launch.
        if named && first == Some(b"--probe".as_slice()) {
            let (Some(&out), Some(&exe)) = (args.get(2), args.get(3)) else {
                die(b"usage: brokkr-launch --probe OUT EXE [ARGS...]", b"", 0);
            };
            probe(out, exe, envp);
        }
        if named && first == Some(b"--plan".as_slice()) {
            let (Some(&plan), Some(&exe)) = (args.get(2), args.get(3)) else {
                die(b"usage: brokkr-launch --plan PLAN EXE [ARGS...]", b"", 0);
            };
            plan_mode(plan, exe, args.get(4..).unwrap_or(&[]), envp);
        }
        let me = self_path();
        if me.rsplit(|&b| b == b'/').next() == Some(b"brokkr-launch".as_slice()) {
            die(b"usage: brokkr-launch --plan PLAN EXE [ARGS...] | --probe OUT EXE [ARGS...]", b"", 0);
        }
        peer_mode(me, args.get(1..).unwrap_or(&[]), envp)
    }

    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _start() -> ! {
        naked_asm!("mov rdi, rsp", "and rsp, -16", "call {entry}", "ud2", entry = sym entry)
    }

    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _start() -> ! {
        naked_asm!("mov x0, sp", "and sp, x0, #-16", "bl {entry}", "brk #0", entry = sym entry)
    }

    // -----------------------------------------------------------------------
    // What libc would otherwise supply
    // -----------------------------------------------------------------------

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
        die(b"internal error", b"", 0)
    }

    use core::ffi::c_void;

    /// core is precompiled with unwind tables that name this symbol. With
    /// `panic=abort` nothing unwinds, so it is never called.
    #[unsafe(no_mangle)]
    extern "C" fn rust_eh_personality() {}

    // Volatile accesses keep the compiler from recognizing these loops as the
    // very functions they implement and calling them recursively.

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcpy(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
        for i in 0..n {
            // SAFETY: the caller guarantees both ranges.
            unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
        }
        dst
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memmove(dst: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        let (d, s) = (dst.cast::<u8>(), src.cast::<u8>());
        if d.addr() < s.addr() {
            for i in 0..n {
                // SAFETY: the caller guarantees both ranges.
                unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
            }
        } else {
            for i in (0..n).rev() {
                // SAFETY: as above.
                unsafe { d.add(i).write_volatile(s.add(i).read_volatile()) };
            }
        }
        dst
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
        let d = dst.cast::<u8>();
        let byte = c.to_le_bytes()[0];
        for i in 0..n {
            // SAFETY: the caller guarantees the range.
            unsafe { d.add(i).write_volatile(byte) };
        }
        dst
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
        let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
        for i in 0..n {
            // SAFETY: the caller guarantees both ranges.
            let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
            if x != y {
                return i32::from(x) - i32::from(y);
            }
        }
        0
    }

    #[unsafe(no_mangle)]
    unsafe extern "C" fn bcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
        // SAFETY: same contract as memcmp.
        unsafe { memcmp(a, b, n) }
    }
}
