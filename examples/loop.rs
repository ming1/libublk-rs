use anyhow::Result;
use io_uring::{opcode, squeue, types};
use libublk::dev_flags::*;
use libublk::io::{UblkDev, UblkIOCtx, UblkQueue};
use libublk::{ctrl::UblkCtrl, UblkError, UblkIORes};
use log::trace;
use serde::Serialize;
use std::os::unix::io::AsRawFd;

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

fn loop_queue_tgt_io(q: &UblkQueue, tag: u16, _io: &UblkIOCtx) {
    // either start to handle or retry
    let _iod = q.get_iod(tag);
    let iod = unsafe { &*_iod };

    let off = (iod.start_sector << 9) as u64;
    let bytes = (iod.nr_sectors << 9) as u32;
    let op = iod.op_flags & 0xff;
    let data = UblkIOCtx::build_user_data(tag as u16, op, 0, true);
    let buf_addr = q.get_io_buf_addr(tag);

    if op == libublk::sys::UBLK_IO_OP_WRITE_ZEROES || op == libublk::sys::UBLK_IO_OP_DISCARD {
        q.complete_io_cmd(tag, Err(UblkError::OtherError(-libc::EINVAL)));
        return;
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
        _ => q.complete_io_cmd(tag, Err(UblkError::OtherError(-libc::EINVAL))),
    };
}

fn _lo_handle_io(q: &UblkQueue, tag: u16, i: &UblkIOCtx) {
    // our IO on backing file is done
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

    loop_queue_tgt_io(q, tag, i);
}

fn test_add() {
    let back_file = std::env::args().nth(3).unwrap();
    let _pid = unsafe { libc::fork() };

    if _pid == 0 {
        // LooTgt has to live in the whole device lifetime
        let lo = LoopTgt {
            back_file: std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&back_file)
                .unwrap(),
            direct_io: 1,
            back_file_path: back_file.clone(),
        };
        let wh = {
            let sess = libublk::UblkSessionBuilder::default()
                .name("loop")
                .dev_flags(UBLK_DEV_F_ADD_DEV)
                .build()
                .unwrap();

            let tgt_init = |dev: &mut UblkDev| lo_init_tgt(dev, &lo);
            let (mut ctrl, dev) = sess.create_devices(tgt_init).unwrap();
            let q_fn = move |qid: u16, _dev: &UblkDev| {
                let lo_io_handler =
                    move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| _lo_handle_io(q, tag, io);

                UblkQueue::new(qid, _dev)
                    .unwrap()
                    .wait_and_handle_io(lo_io_handler);
            };

            sess.run_target(&mut ctrl, &dev, q_fn, |dev_id| {
                let mut d_ctrl = UblkCtrl::new_simple(dev_id, 0).unwrap();
                d_ctrl.dump();
            })
            .unwrap()
        };
        wh.join().unwrap();
    }
}

fn test_del() {
    let s = std::env::args().nth(2).unwrap_or_else(|| "0".to_string());
    let dev_id = s.parse::<i32>().unwrap();
    let mut ctrl = UblkCtrl::new_simple(dev_id as i32, 0).unwrap();

    ctrl.del().unwrap();
}

fn main() {
    if let Some(cmd) = std::env::args().nth(1) {
        match cmd.as_str() {
            "add" => test_add(),
            "del" => test_del(),
            _ => todo!(),
        }
    }
}
