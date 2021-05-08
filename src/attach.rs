use crate::device::Block;
use crate::result::Result;
use event_manager::EventManager;
use event_manager::MutEventSubscriber;
use log::*;
use nix::unistd::Pid;
use simple_error::{require_with, try_with};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use vm_device::bus::MmioAddress;
use vm_virtio::device::VirtioDevice;
use vm_virtio::device::WithDriverSelect;
use vm_virtio::Queue;

use crate::device::{Device, DEVICE_MAX_MEM};
use crate::kvm::{self, hypervisor::Hypervisor};
use crate::tracer::wrap_syscall::KvmRunWrapper;

// Arc<Mutex<>> because the same device (a dyn DevicePio/DeviceMmio from IoManager's
// perspective, and a dyn MutEventSubscriber from EventManager's) is managed by the 2 entities,
// and isn't Copy-able; so once one of them gets ownership, the other one can't anymore.
pub type SubscriberEventManager = EventManager<Arc<Mutex<dyn MutEventSubscriber + Send>>>;

pub struct AttachOptions {
    pub pid: Pid,
    pub ssh_args: Vec<String>,
    pub backing: PathBuf,
}

// We don't need deep stacks for our driver so let's safe a bit memory
const DEFAULT_THREAD_STACKSIZE: usize = 8 * 4096;

struct DeviceReady {
    // supress warning because of https://github.com/rust-lang/rust-clippy/issues/1516
    #[allow(clippy::mutex_atomic)]
    lock: Mutex<bool>,
    condvar: Condvar,
}

// supress warning because of https://github.com/rust-lang/rust-clippy/issues/1516
#[allow(clippy::mutex_atomic)]
impl DeviceReady {
    fn new() -> Self {
        DeviceReady {
            lock: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }
    fn notify(&self) -> Result<()> {
        let mut started = try_with!(self.lock.lock(), "failed to lock");
        *started = true;
        self.condvar.notify_all();
        Ok(())
    }

    fn wait(&self) -> Result<()> {
        let mut started = try_with!(self.lock.lock(), "failed to lock");
        while !*started {
            started = try_with!(self.condvar.wait(started), "failed to wait for condvar");
        }
        Ok(())
    }
}

const STAGE1_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/stage1.ko"));

fn spawn_stage1(
    ssh_args: &[String],
    device_ready: &Arc<DeviceReady>,
) -> Result<JoinHandle<Result<()>>> {
    let builder = thread::Builder::new()
        .name(String::from("stage1"))
        .stack_size(DEFAULT_THREAD_STACKSIZE);

    let ssh_args = ssh_args.to_vec();

    let device_ready = Arc::clone(device_ready);
    Ok(try_with!(
        builder.spawn(move || {
            // wait until vmsh can process block device requests
            device_ready.wait()?;
            let mut child = try_with!(
                Command::new("ssh")
                    .stdin(Stdio::piped())
                    .arg("-oStrictHostKeyChecking=no")
                    .arg("-oUserKnownHostsFile=/dev/null")
                    .args(ssh_args)
                    .arg("--")
                    .arg("sh")
                    .arg("-c")
                    .arg(
                        r#"
set -eux -o pipefail
tmpfile=$(mktemp --suffix .ko)
trap "rm -f $tmpfile" EXIT
cat > $tmpfile
insmod $tmpfile
"#
                    )
                    .spawn(),
                "ssh command failed"
            );

            let mut stdin = require_with!(child.stdin.take(), "Failed to open stdin");
            try_with!(stdin.write_all(STAGE1_EXE), "Failed to write to stdin");

            try_with!(child.wait(), "Failed to load stage1 kernel driver");
            Ok(())
        }),
        "failed to create stage1 thread"
    ))
}

pub fn attach(opts: &AttachOptions) -> Result<()> {
    info!("attaching");

    let vm = Arc::new(try_with!(
        kvm::hypervisor::get_hypervisor(opts.pid),
        "cannot get vms for process {}",
        opts.pid
    ));
    vm.stop()?;

    let mut event_manager = try_with!(SubscriberEventManager::new(), "cannot create event manager");
    // TODO add subscriber (wrapped_exit_handler) and stuff?

    // instantiate blkdev
    let device = try_with!(
        Device::new(&vm, &mut event_manager, &opts.backing),
        "cannot create vm"
    );
    info!("mmio dev attached");
    event_thread(event_manager)?;

    let child = blkdev_monitor_thread(&device)?;

    let device_ready = Arc::new(DeviceReady::new());

    try_with!(spawn_stage1(&opts.ssh_args, &device_ready), "stage1 failed");

    // run guest until driver has inited
    try_with!(
        run_kvm_wrapped(&vm, &device, &device_ready),
        "device init stage with KvmRunWrapper failed"
    );

    info!("blkdev queue ready.");
    vm.resume()?;

    info!("pause");
    nix::unistd::pause();
    let _err = child.join();
    Ok(())
}

fn event_thread(mut event_mgr: SubscriberEventManager) -> Result<JoinHandle<()>> {
    let builder = thread::Builder::new()
        .name(String::from("event-manager"))
        .stack_size(DEFAULT_THREAD_STACKSIZE);
    Ok(try_with!(
        builder.spawn(move || loop {
            match event_mgr.run() {
                Ok(nr) => log::debug!("EventManager: processed {} events", nr),
                Err(e) => log::warn!("Failed to handle events: {:?}", e),
            }
            // TODO if !self.exit_handler.keep_running() { break; }
        }),
        "failed to spawn event-manager thread"
    ))
}

fn exit_condition(_blkdev: &Arc<Mutex<Block>>) -> Result<bool> {
    Ok(false)
    // TODO think of teardown
    //let blkdev = &try_with!(blkdev.lock(), "cannot get blkdev lock");
    //Ok(blkdev.selected_queue().map(|q| q.ready).unwrap())
}

fn blkdev_monitor_thread(device: &Device) -> Result<JoinHandle<()>> {
    let builder = thread::Builder::new()
        .name(String::from("blkdev-monitor"))
        .stack_size(DEFAULT_THREAD_STACKSIZE);
    let blkdev = device.blkdev.clone();
    Ok(try_with!(
        builder.spawn(move || loop {
            {
                match exit_condition(&blkdev) {
                    Err(e) => warn!("cannot evaluate exit condition: {}", e),
                    Ok(b) => {
                        if b {
                            break;
                        }
                    }
                }
                let blkdev = blkdev.lock().unwrap();
                info!("");
                info!("dev type {}", blkdev.device_type());
                info!("dev features b{:b}", blkdev.device_features());
                info!(
                    "dev interrupt stat b{:b}",
                    blkdev
                        .interrupt_status()
                        .load(std::sync::atomic::Ordering::Relaxed)
                );
                info!("dev status b{:b}", blkdev.device_status());
                info!("dev config gen {}", blkdev.config_generation());
                info!(
                    "dev selqueue max size {}",
                    blkdev.selected_queue().map(Queue::max_size).unwrap()
                );
                info!(
                    "dev selqueue ready {}",
                    blkdev.selected_queue().map(|q| q.ready).unwrap()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }),
        "failed to spawn blkdev-monitor"
    ))
}

/// returns when blkdev queue is ready
fn run_kvm_wrapped(
    vm: &Arc<Hypervisor>,
    device: &Device,
    device_ready: &Arc<DeviceReady>,
) -> Result<()> {
    let mut mmio_mgr = device.mmio_mgr.lock().unwrap();
    let device_ready = Arc::clone(device_ready);

    vm.kvmrun_wrapped(|wrapper_mo: &Mutex<Option<KvmRunWrapper>>| {
        let mmio_space = {
            let blkdev = device.blkdev.clone();
            let blkdev = &try_with!(blkdev.lock(), "TODO");
            blkdev.mmio_cfg.range
        };

        // Notify stage0 set up blockdevice driver
        device_ready.notify()?;

        loop {
            let mut kvm_exit;
            {
                let mut wrapper_go = try_with!(wrapper_mo.lock(), "cannot obtain wrapper mutex");
                let wrapper_g = wrapper_go.as_mut().unwrap(); // kvmrun_wrapped guarentees Some()
                kvm_exit = try_with!(
                    wrapper_g.wait_for_ioctl(),
                    "failed to wait for vmm exit_mmio"
                );
            }

            if let Some(mmio_rw) = &mut kvm_exit {
                let addr = MmioAddress(mmio_rw.addr);
                let from = mmio_space.base();
                // virtio mmio space + virtio device specific space
                let to = from + mmio_space.size() + DEVICE_MAX_MEM;
                if from <= addr && addr < to {
                    // intercept op
                    debug!("mmio access 0x{:x}", addr.0);
                    try_with!(mmio_mgr.handle_mmio_rw(mmio_rw), "failed to handle MmioRw");
                } else {
                    // do nothing, just continue to ingore and pass to hv
                }
                if exit_condition(&device.blkdev)? {
                    break;
                }
            }
        }
        Ok(())
    })?;
    Ok(())
}
