use libublk::dev_flags::*;
use libublk::io::{UblkDev, UblkIOCtx, UblkQueue};
use libublk::{ctrl::UblkCtrl, UblkError, UblkIORes};

fn handle_io(q: &UblkQueue, tag: u16, _io: &UblkIOCtx, start: u64) {
    let _iod = q.get_iod(tag);
    let iod = unsafe { &*_iod };
    let off = (iod.start_sector << 9) as u64;
    let bytes = (iod.nr_sectors << 9) as i32;
    let op = iod.op_flags & 0xff;
    let buf_addr = q.get_io_buf_addr(tag);

    match op {
        libublk::sys::UBLK_IO_OP_READ => unsafe {
            libc::memcpy(
                buf_addr as *mut libc::c_void,
                (start + off) as *mut libc::c_void,
                bytes as usize,
            );
        },
        libublk::sys::UBLK_IO_OP_WRITE => unsafe {
            libc::memcpy(
                (start + off) as *mut libc::c_void,
                buf_addr as *mut libc::c_void,
                bytes as usize,
            );
        },
        _ => {
            q.complete_io_cmd(tag, Err(UblkError::OtherError(-libc::EINVAL)));
            return;
        }
    }

    q.complete_io_cmd(tag, Ok(UblkIORes::Result(bytes)));
}

///run this ramdisk ublk daemon completely in single context with
///async control command, no need Rust async any more
fn rd_add_dev(dev_id: i32, buf_addr: u64, size: u64, for_add: bool) {
    let dev_flags = if for_add {
        UBLK_DEV_F_ADD_DEV
    } else {
        UBLK_DEV_F_RECOVER_DEV
    };

    let wh = {
        let sess = libublk::UblkSessionBuilder::default()
            .name("example_ramdisk")
            .id(dev_id)
            .nr_queues(1_u16)
            .depth(128_u16)
            .dev_flags(dev_flags)
            .ctrl_flags(libublk::sys::UBLK_F_USER_RECOVERY as u64)
            .build()
            .unwrap();

        let tgt_init = |dev: &mut UblkDev| {
            dev.set_default_params(size);
            Ok(serde_json::json!({}))
        };
        let (mut ctrl, dev) = sess.create_devices(tgt_init).unwrap();
        let q_fn = move |qid: u16, _dev: &UblkDev| {
            let rd_io_handler = move |q: &UblkQueue, tag: u16, io: &UblkIOCtx| {
                handle_io(q, tag, io, buf_addr);
            };
            UblkQueue::new(qid, _dev, true)
                .unwrap()
                .wait_and_handle_io(rd_io_handler);
        };

        sess.run_target(&mut ctrl, &dev, q_fn, |dev_id| {
            let mut d_ctrl = UblkCtrl::new_simple(dev_id, 0).unwrap();
            d_ctrl.dump();
        })
        .unwrap()
    };
    wh.join().unwrap();
}

fn rd_get_device_size(ctrl: &mut UblkCtrl) -> u64 {
    if let Ok(tgt) = ctrl.get_target_from_json() {
        tgt.dev_size
    } else {
        0
    }
}

fn test_add(recover: usize) {
    let dev_id: i32 = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "-1".to_string())
        .parse::<i32>()
        .unwrap();
    let s = std::env::args().nth(3).unwrap_or_else(|| "32".to_string());
    let mb = s.parse::<u64>().unwrap();

    let _pid = unsafe { libc::fork() };
    if _pid == 0 {
        let mut size = (mb << 20) as u64;

        if recover > 0 {
            assert!(dev_id >= 0);
            let mut ctrl = UblkCtrl::new_simple(dev_id, 0).unwrap();
            size = rd_get_device_size(&mut ctrl);

            ctrl.start_user_recover().unwrap();
        }
        let buf = libublk::ublk_alloc_buf(size as usize, 4096);

        rd_add_dev(dev_id, buf as u64, size, recover == 0);

        libublk::ublk_dealloc_buf(buf, size as usize, 4096);
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
            "add" => test_add(0),
            "recover" => test_add(1),
            "del" => test_del(),
            _ => todo!(),
        }
    }
}
