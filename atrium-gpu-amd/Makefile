# atrium-gpu-amd — §4.1 three-module split, built IN the FreeBSD VM (kld modules
# are KBI-bound to the running kernel; never cross-compile). Sources stay in this
# directory; each subdir is just a Makefile that .PATH's back here.
#
#   base/    atrium_gpu_amd_pci.ko     — PCI bring-up + BARs + MSI-X + shared softc
#   gpu/     atrium_gpu_amd.ko         — compute/render, /dev/atrium-gpu0  (DEPEND pci)
#   display/ atrium_gpu_amd_display.ko — display engine, /dev/atrium-display0 (DEPEND pci)
#
#   cd /mnt/host/atrium-gpu-amd && make
#   kldload base/atrium_gpu_amd_pci.ko gpu/atrium_gpu_amd.ko \
#           display/atrium_gpu_amd_display.ko
#
# Requires the device present: boot the VM with `run-vm.sh --gpusim`.

SUBDIR=	base gpu display

.include <bsd.subdir.mk>
