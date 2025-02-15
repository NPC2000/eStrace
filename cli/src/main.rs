use anyhow::Result;
use aya::maps::AsyncPerfEventArray;
use aya::programs::TracePoint;
use aya::util::online_cpus;
use aya::{include_bytes_aligned, Bpf};
use bytes::{Buf, BytesMut};
use clap::Parser;
use colored::Colorize;
use common::STR_MAX_LENGTH;
use std::collections::HashMap;
use tokio::sync::Mutex;

use crate::event::Event;

#[cfg(target_arch = "aarch64")]
pub use syscalls::aarch64::Sysno;
#[cfg(target_arch = "x86_64")]
pub use syscalls::x86_64::Sysno;

mod event;
mod filter;
mod syscall_info;

#[derive(Parser)]
pub struct Args {
    #[command(flatten)]
    include: Option<IncludeArg>,
    #[command(flatten)]
    exclude: Option<ExcludeArg>,

    /// Capture all process
    #[arg(long)]
    all: bool,
    /// Trace only the specified set of system calls.  syscall_set is defined as [!]value[,value], and value can be one of the following:
    ///
    /// syscall      Trace specific syscall, specified by its name (see syscalls(2) for a reference, but also see NOTES).
    ///
    /// all          Trace all system calls.
    ///
    /// /regex       Trace only those system calls that match the regex.
    ///
    /// %file        Trace all system calls which take a file name as an argument.  You can think of this as an abbreviation for -e trace=open,stat,hmod,unlink,...  which is useful to seeing what files  the  process  is  referencing.  Furthermore, using the abbreviation will ensure that you don't accidentally forget to include a call like lstat(2) in the list.  Betchya woulda forgot that one.  The syntax without a preceding percent
    ///
    /// %process     Trace system calls associated with process lifecycle (creation, exec, termination).
    ///
    /// %net         Trace all the network related system calls.  The syntax without a preceding percent
    ///
    /// %signal      Trace all signal related system calls.  The syntax without a preceding percent
    ///
    /// %ipc         Trace all IPC related system calls.  The syntax without a preceding percent
    ///
    /// %desc        Trace all file descriptor related system calls.  The syntax without a preceding percent
    ///
    /// %memory      Trace all memory mapping related system calls.  The syntax without a preceding percent
    ///
    /// %creds       Trace system calls that read or modify user and group identifiers or capability sets.
    ///
    /// %stat        Trace stat syscall variants.
    ///
    /// %lstat       Trace lstat syscall variants.
    ///
    /// %fstat       Trace fstat, fstatat, and statx syscall variants.
    ///
    /// %%stat       Trace syscalls used for requesting file status (stat, lstat, fstat, fstatat, statx, and their variants).
    ///
    /// %statfs      Trace statfs, statfs64, statvfs, osf_statfs, and osf_statfs64 system calls.  The same effect can be achieved with -e trace=/^(.*_)?statv?fs regular expression.
    ///
    /// %fstatfs     Trace fstatfs, fstatfs64, fstatvfs, osf_fstatfs, and osf_fstatfs64 system calls.  The same effect can be achieved with -e trace=/fstatv?fs regular expression.
    ///
    /// %%statfs     Trace syscalls related to file system statistics (statfs-like, fstatfs-like, and ustat).  The same effect can be achieved with -e trace=/statv?fs|fsstat|ustat regular  expression.
    ///
    /// %clock       Trace system calls that read or modify system clocks.
    ///
    /// %pure        Trace  syscalls  that always succeed and have no arguments.  Currently, this list includes arc_gettls(2), getdtablesize(2), getegid(2), getegid32(2), geteuid(2), geteuid32(2),
    ///             getgid(2), getgid32(2), getpagesize(2), getpgrp(2), getpid(2), getppid(2), get_thread_area(2) (on architectures other than x86), gettid(2), get_tls(2), getuid(2), getuid32(2),
    ///             getxgid(2), getxpid(2), getxuid(2), kern_features(2), and metag_get_tls(2) syscalls.
    ///
    /// %seccomp_default Trace seccomp default actions.
    #[clap(short, long)]
    filter: Option<String>,
}

#[derive(clap::Args, Clone)]
#[group(required = false, multiple = true)]
struct IncludeArg {
    /// Target pid
    #[clap(short, long)]
    pid: Vec<u32>,

    /// Target tid
    #[clap(short, long)]
    tid: Vec<u32>,

    /// Target uid
    #[clap(short, long)]
    uid: Vec<u32>,
}

#[derive(clap::Args, Clone)]
#[group(required = false, multiple = true)]
struct ExcludeArg {
    /// Exclude pid
    #[clap(long)]
    exclude_pid: Vec<u32>,

    /// Exclude tid
    #[clap(long)]
    exclude_tid: Vec<u32>,

    /// Exclude uid
    #[clap(long)]
    exclude_uid: Vec<u32>,

    #[clap(long)]
    /// Exclude the estrace itself
    exclude_self: bool,
}

#[derive(Debug)]
struct BpfArg {
    exclude_mode: bool,
    pid: Vec<u32>,
    tid: Vec<u32>,
    uid: Vec<u32>,
    filiter: filter::Filter,
}

lazy_static::lazy_static! {
    static ref PROCESSING_EVENTS: Mutex<HashMap<u32, Event>> = Mutex::new(HashMap::new());
}

async fn handle_event(byte: &mut BytesMut) {
    let ty = byte.get_u8();
    let tid = byte.get_u32_le();
    let pid = byte.get_u32_le();
    let syscall_or_arg = byte.get_u64_le();
    let addition_size = byte.get_u16_le() as usize;
    if ty & 0xf0 == 0 {
        PROCESSING_EVENTS.lock().await.insert(
            tid,
            Event {
                tid,
                pid,
                syscall: syscall_or_arg,
                return_value: None,
                args: [None, None, None, None, None, None],
            },
        );
    } else if ty & 0xf0 == 0x10 {
        if let Some(w) = PROCESSING_EVENTS.lock().await.get_mut(&tid) {
            w.args[(ty & 0xf) as usize] =
                Some((syscall_or_arg, byte.split_to(addition_size).freeze()));
        }
    } else if ty & 0xf0 == 0x20 {
        if let Some(mut w) = PROCESSING_EVENTS.lock().await.remove(&tid) {
            if addition_size != 8 {
                return;
            }
            w.return_value = Some(byte.get_u64_le());
            println!("{}", w);
        }
    }
}

fn init_bpf(args: &BpfArg) -> Result<Bpf> {
    #[cfg(debug_assertions)]
    let bpf_data = include_bytes_aligned!("../../target/bpfel-unknown-none/debug/ebpf");
    #[cfg(not(debug_assertions))]
    let bpf_data = include_bytes_aligned!("../../target/bpfel-unknown-none/release/ebpf");
    let mut bpf = Bpf::load(bpf_data)?;

    let mut syscall_arg_table: aya::maps::HashMap<_, u64, [u16; 6]> =
        bpf.take_map("SYSCALL_ARG_TABLE").unwrap().try_into()?;
    for (sysno, v) in syscall_info::arch::SYSCALL_ARG_TABLE.iter().enumerate() {
        if !args.filiter.check(sysno as u64) {
            continue;
        }
        let mut map_element: [u16; 6] = [0; 6];
        for (i, element) in v.args.iter().enumerate() {
            map_element[i] = ((element.0.bits() as u16) << 8) | element.1 as u16;
        }
        syscall_arg_table.insert(sysno as u64, map_element, 0)?;
    }

    if args.exclude_mode {
        let mut map: aya::maps::HashMap<_, u8, u8> = bpf.take_map("FLAG").unwrap().try_into()?;
        map.insert(0, 0, 0)?;
        std::mem::forget(map);
    }
    if !args.pid.is_empty() {
        let mut map: aya::maps::HashMap<_, u32, u8> =
            bpf.take_map("TRAGET_PID").unwrap().try_into()?;
        for pid in &args.pid {
            map.insert(pid, 0, 0)?;
        }
        std::mem::forget(map);
    }
    if !args.tid.is_empty() {
        let mut map: aya::maps::HashMap<_, u32, u8> =
            bpf.take_map("TRAGET_TID").unwrap().try_into()?;
        for tid in &args.tid {
            map.insert(tid, 0, 0)?;
        }
        std::mem::forget(map);
    }
    if !args.uid.is_empty() {
        let mut map: aya::maps::HashMap<_, u32, u8> =
            bpf.take_map("TRAGET_UID").unwrap().try_into()?;
        for uid in &args.uid {
            map.insert(uid, 0, 0)?;
        }
        std::mem::forget(map);
    }

    let mut record: AsyncPerfEventArray<_> = bpf.take_map("RECORD_LOGS").unwrap().try_into()?;

    for cpu_id in online_cpus()? {
        let mut buf = record.open(cpu_id, None)?;
        tokio::spawn(async move {
            let mut buffers = vec![BytesMut::with_capacity(19 + STR_MAX_LENGTH); 512];

            loop {
                let events = buf.read_events(&mut buffers).await.unwrap();
                if events.lost != 0 {
                    println!("{} Lost {} events", "Warning:".yellow().bold(), events.lost);
                }
                for i in buffers.iter_mut().take(events.read) {
                    handle_event(i).await;
                }
            }
        });
    }

    let program: &mut TracePoint = bpf.program_mut("enter_syscall").unwrap().try_into()?;
    program.load()?;
    program.attach("raw_syscalls", "sys_enter")?;

    let program: &mut TracePoint = bpf.program_mut("exit_syscall").unwrap().try_into()?;
    program.load()?;
    program.attach("raw_syscalls", "sys_exit")?;
    Ok(bpf)
}

fn bump_memlock_rlimit() {
    // Bump the memlock rlimit. This is needed for older kernels that don't use the
    // new memcg based accounting, see https://lwn.net/Articles/837122/
    let rlimit = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };

    let ret = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &rlimit) };
    if ret != 0 {
        println!(
            "{} remove limit on locked memory failed, ret is: {}",
            "Warning:".yellow().bold(),
            ret
        );
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = match parse_bpf_arg(Args::parse()) {
        Ok(args) => args,
        Err(e) => {
            println!("{} {}", "Error:".red().bold(), e);
            std::process::exit(1);
        }
    };
    bump_memlock_rlimit();
    let bpf = init_bpf(&args)?;
    tokio::signal::ctrl_c().await?;
    std::mem::drop(bpf);
    Ok(())
}

fn parse_bpf_arg(args: Args) -> Result<BpfArg, String> {
    let filter = filter::Filter::new(args.filter.as_deref())?;
    match (args.include, args.exclude, args.all) {
        (Some(include), None, false) => Ok(BpfArg {
            exclude_mode: false,
            tid: include.tid,
            uid: include.uid,
            pid: include.pid,
            filiter: filter,
        }),
        (None, Some(exclude), false) => {
            let mut arg = BpfArg {
                exclude_mode: true,
                tid: exclude.exclude_tid,
                uid: exclude.exclude_uid,
                pid: exclude.exclude_pid,
                filiter: filter,
            };
            if exclude.exclude_self {
                arg.pid.push(std::process::id())
            }
            Ok(arg)
        }
        (None, None, true) => Ok(BpfArg {
            exclude_mode: true,
            tid: vec![],
            uid: vec![],
            pid: vec![],
            filiter: filter,
        }),
        (None, None, false) => Err(
            "You must specify filtering rules. If you want to monitor all processes, use `--all`"
                .to_string(),
        ),
        _ => Err(
            "There can be only one parameter for inclusion and exclusion types and `--all`"
                .to_string(),
        ),
    }
}
