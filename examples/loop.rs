use anyhow::Result;
use clap::{Arg, ArgAction, Command};
use io_uring::{opcode, squeue, types};
use libublk::dev_flags::*;
use libublk::io::{UblkDev, UblkIOCtx, UblkQueue};
use libublk::{
    ctrl::UblkCtrl, exe::Executor, exe::UringOpFuture, UblkError, UblkIORes, UblkSession,
};
use log::trace;
use serde::Serialize;
use std::os::unix::io::AsRawFd;
use std::rc::Rc;

#[derive(Debug, Serialize)]
struct LoJson {
    back_file_path: String,
    direct_io: i32,
}

struct LoopTgt {
    back_file_path: String,
    back_file: std::fs::File,
    direct_io: i32,
}

fn lo_file_size(f: &std::fs::File) -> Result<u64> {
    if let Ok(meta) = f.metadata() {
        if meta.file_type().is_file() {
            Ok(f.metadata().unwrap().len())
        } else {
            Err(anyhow::anyhow!("unsupported file"))
        }
    } else {
        Err(anyhow::anyhow!("no file meta got"))
    }
}

// setup loop target
fn lo_init_tgt(dev: &mut UblkDev, lo: &LoopTgt) -> Result<serde_json::Value, UblkError> {
    trace!("loop: init_tgt {}", dev.dev_info.dev_id);
    if lo.direct_io != 0 {
        unsafe {
            libc::fcntl(lo.back_file.as_raw_fd(), libc::F_SETFL, libc::O_DIRECT);
        }
    }

    let dev_size = {
        let tgt = &mut dev.tgt;
        let nr_fds = tgt.nr_fds;
        tgt.fds[nr_fds as usize] = lo.back_file.as_raw_fd();
        tgt.nr_fds = nr_fds + 1;

        tgt.dev_size = lo_file_size(&lo.back_file).unwrap();
        tgt.dev_size
    };
    dev.set_default_params(dev_size);

    Ok(
        serde_json::json!({"loop": LoJson { back_file_path: lo.back_file_path.clone(), direct_io: 1 } }),
    )
}

#[inline]
fn __lo_handle_io_cmd(
    q: &UblkQueue<'_>,
    tag: u16,
    iod: &libublk::sys::ublksrv_io_desc,
    data: u64,
) -> bool {
    let op = iod.op_flags & 0xff;
    // either start to handle or retry
    let off = (iod.start_sector << 9) as u64;
    let bytes = (iod.nr_sectors << 9) as u32;
    let buf_addr = q.get_io_buf_addr(tag);

    if op == libublk::sys::UBLK_IO_OP_WRITE_ZEROES || op == libublk::sys::UBLK_IO_OP_DISCARD {
        q.complete_io_cmd(tag, Err(UblkError::OtherError(-libc::EINVAL)));
        return true;
    }

    match op {
        libublk::sys::UBLK_IO_OP_FLUSH => {
            let sqe = &opcode::SyncFileRange::new(types::Fixed(1), bytes)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
                .user_data(data);
            unsafe {
                q.q_ring
                    .borrow_mut()
                    .submission()
                    .push(sqe)
                    .expect("submission fail");
            }
        }
        libublk::sys::UBLK_IO_OP_READ => {
            let sqe = &opcode::Read::new(types::Fixed(1), buf_addr, bytes)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
                .user_data(data);
            unsafe {
                q.q_ring
                    .borrow_mut()
                    .submission()
                    .push(sqe)
                    .expect("submission fail");
            }
        }
        libublk::sys::UBLK_IO_OP_WRITE => {
            let sqe = &opcode::Write::new(types::Fixed(1), buf_addr, bytes)
                .offset(off)
                .build()
                .flags(squeue::Flags::FIXED_FILE)
                .user_data(data);
            unsafe {
                q.q_ring
                    .borrow_mut()
                    .submission()
                    .push(sqe)
                    .expect("submission fail");
            }
        }
        _ => {
            q.complete_io_cmd(tag, Err(UblkError::OtherError(-libc::EINVAL)));
            return true;
        }
    };

    return false;
}

async fn lo_handle_io_cmd_async(q: &UblkQueue<'_>, tag: u16) {
    let _iod = q.get_iod(tag);
    let iod = unsafe { &*_iod };
    let op = iod.op_flags & 0xff;
    let data = UblkIOCtx::build_user_data_async(tag as u16, op, 0);
    for i in 0..4 {
        if !__lo_handle_io_cmd(q, tag, iod, data) {
            // wait until the io_uring IO completed
            let res = UringOpFuture { user_data: data }.await;

            if res != -(libc::EAGAIN) || i == 3 {
                q.complete_io_cmd(tag, Ok(UblkIORes::Result(res)));
                break;
            }
        }
    }
}

fn lo_handle_io_cmd_sync(q: &UblkQueue<'_>, tag: u16, i: &UblkIOCtx) {
    let _iod = q.get_iod(tag);
    let iod = unsafe { &*_iod };
    let op = iod.op_flags & 0xff;
    let data = UblkIOCtx::build_user_data(tag as u16, op, 0, true);
    if i.is_tgt_io() {
        let user_data = i.user_data();
        let res = i.result();
        let cqe_tag = UblkIOCtx::user_data_to_tag(user_data);

        assert!(cqe_tag == tag as u32);

        if res != -(libc::EAGAIN) {
            q.complete_io_cmd(tag, Ok(UblkIORes::Result(res)));
            return;
        }
    }
    __lo_handle_io_cmd(q, tag, iod, data);
}

fn test_add(
    id: i32,
    nr_queues: u32,
    depth: u32,
    buf_sz: u32,
    backing_file: &String,
    ctrl_flags: u64,
    aio: bool,
) {
    let _pid = unsafe { libc::fork() };

    if _pid == 0 {
        __test_add(id, nr_queues, depth, buf_sz, backing_file, ctrl_flags, aio);
    }
}
fn __test_add(
    id: i32,
    nr_queues: u32,
    depth: u32,
    buf_sz: u32,
    backing_file: &String,
    ctrl_flags: u64,
    aio: bool,
) {
    // LooTgt has to live in the whole device lifetime
    let lo = LoopTgt {
        back_file: std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&backing_file)
            .unwrap(),
        direct_io: 1,
        back_file_path: backing_file.clone(),
    };
    let wh = {
        let sess = libublk::UblkSessionBuilder::default()
            .name("example_loop")
            .id(id)
            .ctrl_flags(ctrl_flags)
            .nr_queues(nr_queues)
            .depth(depth)
            .io_buf_bytes(buf_sz)
            .dev_flags(UBLK_DEV_F_ADD_DEV)
            .build()
            .unwrap();

        let tgt_init = |dev: &mut UblkDev| lo_init_tgt(dev, &lo);
        let (mut ctrl, dev) = sess.create_devices(tgt_init).unwrap();
        let q_async_fn = move |qid: u16, dev: &UblkDev| {
            let q_rc = Rc::new(UblkQueue::new(qid as u16, &dev).unwrap());
            let exe_rc = Rc::new(Executor::new(dev.get_nr_ios()));
            let q = q_rc.clone();
            let exe = exe_rc.clone();

            let lo_io_handler = move |tag: u16, _io: &UblkIOCtx| {
                let q = q_rc.clone();

                exe.spawn(tag as u16, async move {
                    lo_handle_io_cmd_async(&q, tag).await;
                });
            };
            q.wait_and_handle_io_cmd(&exe_rc, lo_io_handler, None);
        };

        let q_sync_fn = move |qid: u16, _dev: &UblkDev| {
            let lo_io_handler =
                move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| lo_handle_io_cmd_sync(q, tag, io);
            UblkQueue::new(qid, _dev)
                .unwrap()
                .wait_and_handle_io(lo_io_handler);
        };

        sess.run_target(
            &mut ctrl,
            &dev,
            if aio { q_async_fn } else { q_sync_fn },
            |dev_id| {
                let mut d_ctrl = UblkCtrl::new_simple(dev_id, 0).unwrap();
                d_ctrl.dump();
            },
        )
        .unwrap()
    };
    wh.join().unwrap();
}

fn main() {
    let matches = Command::new("ublk-loop-example")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("add")
                .about("Add ublk device")
                .arg(
                    Arg::new("number")
                        .short('n')
                        .long("number")
                        .default_value("-1")
                        .allow_hyphen_values(true)
                        .help("device id, -1: auto-allocation")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("queues")
                        .long("queues")
                        .short('q')
                        .default_value("1")
                        .help("nr_hw_queues")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("depth")
                        .long("depth")
                        .short('d')
                        .default_value("64")
                        .help("queue depth: max in-flight io commands")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("buf_size")
                        .long("buf_size")
                        .short('b')
                        .default_value("524288")
                        .help("io buffer size")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("unprivileged")
                        .long("unprivileged")
                        .short('p')
                        .action(ArgAction::SetTrue)
                        .help("enable UBLK_F_UN_PRIVILEGED_DEV"),
                )
                .arg(
                    Arg::new("backing_file")
                        .long("backing_file")
                        .short('f')
                        .required(true)
                        .help("backing file")
                        .action(ArgAction::Set),
                )
                .arg(
                    Arg::new("async")
                        .long("async")
                        .short('a')
                        .action(ArgAction::SetTrue)
                        .help("use async/await to handle IO command"),
                ),
        )
        .subcommand(
            Command::new("del").about("Delete ublk device").arg(
                Arg::new("number")
                    .short('n')
                    .long("number")
                    .required(true)
                    .help("device id")
                    .action(ArgAction::Set),
            ),
        )
        .subcommand(Command::new("list").about("List ublk device"))
        .get_matches();

    match matches.subcommand() {
        Some(("add", add_matches)) => {
            let id = add_matches
                .get_one::<String>("number")
                .unwrap()
                .parse::<i32>()
                .unwrap_or(-1);
            let nr_queues = add_matches
                .get_one::<String>("queues")
                .unwrap()
                .parse::<u32>()
                .unwrap_or(1);
            let depth = add_matches
                .get_one::<String>("depth")
                .unwrap()
                .parse::<u32>()
                .unwrap_or(64);
            let buf_size = add_matches
                .get_one::<String>("buf_size")
                .unwrap()
                .parse::<u32>()
                .unwrap_or(52288);
            let backing_file = add_matches.get_one::<String>("backing_file").unwrap();

            let ctrl_flags: u64 = if add_matches.get_flag("unprivileged") {
                libublk::sys::UBLK_F_UNPRIVILEGED_DEV as u64
            } else {
                0
            };
            let aio = if add_matches.get_flag("async") {
                true
            } else {
                false
            };
            test_add(
                id,
                nr_queues,
                depth,
                buf_size,
                backing_file,
                ctrl_flags,
                aio,
            );
        }
        Some(("del", add_matches)) => {
            let id = add_matches
                .get_one::<String>("number")
                .unwrap()
                .parse::<i32>()
                .unwrap_or(-1);
            UblkCtrl::new_simple(id, 0).unwrap().del().unwrap();
        }
        Some(("list", _add_matches)) => UblkSession::for_each_dev_id(|dev_id| {
            UblkCtrl::new_simple(dev_id as i32, 0).unwrap().dump();
        }),
        _ => {
            println!("unsupported command");
        }
    };
}
