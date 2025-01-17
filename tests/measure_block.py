from root import MEASURE_RESULTS
import confmeasure
import measure_helpers as util
from measure_helpers import (
    GUEST_JAVDEV,
    GUEST_QEMUBLK,
    GUEST_QEMU9P,
    GUEST_JAVDEV_MOUNT,
    GUEST_QEMUBLK_MOUNT,
    HOST_SSD,
    run,
)
from qemu import QemuVm

from typing import List, Any, Optional, Callable, Tuple
import re
import json


# overwrite the number of samples to take to a minimum
QUICK = True


def lsblk(vm: QemuVm) -> None:
    term = vm.ssh_cmd(["lsblk"], check=False)
    print(term.stdout)


def hdparm(vm: QemuVm, device: str) -> Optional[float]:
    term = vm.ssh_cmd(["hdparm", "-t", device], check=False)
    if term.returncode != 0:
        return None
    out = term.stdout
    print(out)
    out = re.sub(" +", " ", out).split(" ")
    mb = float(out[5])
    sec = float(out[8])
    return mb / sec


def fio(
    vm: Optional[QemuVm],
    device: str,
    random: bool = False,
    readonly: bool = True,
    iops: bool = False,
    file: bool = False,
) -> Tuple[float, float, float, float]:
    """
    inspired by https://docs.oracle.com/en-us/iaas/Content/Block/References/samplefiocommandslinux.htm
    @param random: random vs sequential
    @param readonly: readonly vs read+write
    @param iops: return iops vs bandwidth
    @param file: target is file vs blockdevice
    @return (read_mean, stddev, write_mean, stdev) in kiB/s
    """
    runtime = 120
    size = 100  # filesize in GB
    if QUICK:
        runtime = 10
        size = 10

    cmd = ["fio"]

    if file:
        cmd += [f"--filename={device}/file", f"--size={size}GB"]
    else:
        cmd += [f"--filename={device}", "--direct=1"]

    if readonly and random:
        cmd += ["--rw=randread"]
    elif not readonly and random:
        # fio/examples adds rwmixread=60 and rwmixwrite=40 here
        cmd += ["--rw=randrw"]
    elif readonly and not random:
        cmd += ["--rw=read"]
    elif not readonly and not random:
        cmd += ["--rw=readwrite"]

    if iops:
        # fio/examples uses 16 here as well
        cmd += ["--bs=4k", "--ioengine=libaio", "--iodepth=64"]
    else:
        cmd += ["--bs=64k", "--ioengine=libaio", "--iodepth=16"]

    cmd += [
        f"--runtime={runtime}",
        "--numjobs=1",
        "--time_based",
        "--group_reporting",
        "--name=generic_name",
        "--eta-newline=1",
    ]

    if readonly:
        cmd += ["--readonly"]

    cmd += ["--output-format=json"]

    print(cmd)
    if vm is None:
        term = run(cmd, check=True)
    else:
        term = vm.ssh_cmd(cmd, check=True)

    out = term.stdout
    # print(out)
    j = json.loads(out)
    read = j["jobs"][0]["read"]
    write = j["jobs"][0]["write"]

    if iops:
        print(
            "IOPS: read",
            read["iops_mean"],
            read["iops_stddev"],
            "write",
            write["iops_mean"],
            write["iops_stddev"],
        )
        return (
            read["iops_mean"],
            read["iops_stddev"],
            write["iops_mean"],
            write["iops_stddev"],
        )
    else:
        print("Bandwidth read", float(read["bw_mean"]) / 1024 / 1024, "GB/s")
        print("Bandwidth write", float(write["bw_mean"]) / 1024 / 1024, "GB/s")
        return (read["bw_mean"], read["bw_dev"], write["bw_mean"], write["bw_dev"])


SIZE = 16
WARMUP = 0
if QUICK:
    WARMUP = 0
    SIZE = 2


# QUICK: 20s else: 5min
def sample(
    f: Callable[[], Optional[float]], size: int = SIZE, warmup: int = WARMUP
) -> List[float]:
    ret = []
    for i in range(0, warmup):
        f()
    for i in range(0, size):
        r = f()
        if r is None:
            return []
        ret += [r]
    return ret


# QUICK: ? else: ~5min
def fio_suite(
    vm: Optional[QemuVm], measurements: Any, device: str, name: str, file: bool = True
) -> Any:
    measurements["fio"][f"{name}-best-case-bw"] = list(
        fio(
            vm,
            device,
            random=False,
            readonly=False,
            iops=False,
            file=file,
        )
    )
    measurements["fio"][f"{name}-worst-case-iops"] = list(
        fio(
            vm,
            device,
            random=True,
            readonly=False,
            iops=True,
            file=file,
        )
    )
    pass


# not quick: 5 * fio_suite(5min) + 2 * sample(5min) = 35min
if __name__ == "__main__":
    util.check_ssd()
    util.check_system()
    helpers = confmeasure.Helpers()

    measurements = util.read_stats(f"{MEASURE_RESULTS}/stats.json")
    measurements["fio"] = {}
    measurements["hdparm"] = {}

    # fresh ssd, unmount and fio_suite(HOST_SSD)
    with util.fresh_ssd():
        pass
    fio_suite(None, measurements, HOST_SSD, "direct_host", file=False)

    with util.fresh_ssd():
        with util.testbench(helpers, with_vmsh=False, ioregionfd=False) as vm:
            fio_suite(
                vm, measurements, GUEST_QEMUBLK, "direct_detached_qemublk", file=False
            )
    # testbench wants to mount again -> restore fs via fresh_ssd()
    with util.fresh_ssd():
        with util.testbench(helpers, with_vmsh=True, ioregionfd=False) as vm:
            fio_suite(vm, measurements, GUEST_QEMUBLK, "direct_ws_qemublk", file=False)
            fio_suite(vm, measurements, GUEST_JAVDEV, "direct_ws_javdev", file=False)
    with util.fresh_ssd():
        with util.testbench(helpers, with_vmsh=True, ioregionfd=True) as vm:
            fio_suite(
                vm, measurements, GUEST_QEMUBLK, "direct_iorefd_qemublk", file=False
            )
            fio_suite(
                vm, measurements, GUEST_JAVDEV, "direct_iorefd_javdev", file=False
            )

    # we just wrote randomly to the disk -> fresh_ssd() required
    with util.fresh_ssd():
        with util.testbench(helpers, with_vmsh=False, ioregionfd=False) as vm:
            lsblk(vm)
            fio_suite(vm, measurements, GUEST_QEMUBLK_MOUNT, "detached_qemublk")
            fio_suite(vm, measurements, GUEST_QEMU9P, "detached_qemu9p")
            measurements["hdparm"]["detached_qemublk"] = sample(
                lambda: hdparm(vm, GUEST_QEMUBLK)
            )

        with util.testbench(helpers, with_vmsh=True, ioregionfd=False) as vm:
            fio_suite(vm, measurements, GUEST_JAVDEV_MOUNT, "attached_ws_javdev")

        with util.testbench(helpers, with_vmsh=True, ioregionfd=True) as vm:
            fio_suite(vm, measurements, GUEST_JAVDEV_MOUNT, "attached_iorefd_javdev")

            measurements["hdparm"]["attached_iorefd_javdev"] = sample(
                lambda: hdparm(vm, GUEST_JAVDEV)
            )

    util.export_lineplot("hdparm_warmup", measurements["hdparm"])
    util.export_barplot("hdparm_warmup_barplot", measurements["hdparm"])
    util.export_fio("fio", measurements["fio"])
    util.write_stats(f"{MEASURE_RESULTS}/stats.json", measurements)

"""
# Compare block devices:

- qemu virtio blk
- qemu virtio 9p
- vmsh virtio blk ws
- vmsh virtio blk ioregionfd

for each:
- best case bw read
- best case bw write
- worst case iops

# Compare guest performance under vmsh

- native
- detached
- vmsh ws
- vmsh ioregionfd
- run via vmsh (in container in vm)
- run via ssh (no container in vm)

for each:
- blkdev
- shell latency
- phoronix
"""
