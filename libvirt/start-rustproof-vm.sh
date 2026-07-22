#!/bin/bash
# Boot the Rustproof nucleus as a KVM guest on shark-a with the gfx1201 passed
# through. This is a thin fork of gist-tri-os/start-gpu-vm.sh: the VFIO bind +
# the no-FLR reset_method trick are reused VERBATIM (they preserve the VBIOS POST
# state the gfx1201 PSP/SMU/GC bring-up depends on). The only differences from the
# tri-OS script are: (1) VM defaults to the rustproof-gpu domain, and (2) the guest
# has no SSH, so instead of waiting for SSH we tail the guest serial console for the
# M0 success banner "M0: WAVE OK".
#
# Prereqs: build the boot image first (cargo xtask image) and point the
# rustproof-gpu domain at it. Set these for your machine (secrets in a gitignored
# env file, never in the script):
#   GPU        - gfx1201 PCI BDF (lspci -nn | grep -i 1002:7551)
#   GPU_AUDIO  - HDMI-audio function on the same card (usually .1)
#   VM         - libvirt domain name (default: rustproof-gpu)
#   SERIAL_LOG - host file the guest serial is wired to (domain XML <serial>)

set -e

GPU="${GPU:-0000:c3:00.0}"
GPU_AUDIO="${GPU_AUDIO:-0000:c3:00.1}"
VM="${VM:-rustproof-gpu}"
SERIAL_LOG="${SERIAL_LOG:-/var/log/libvirt/qemu/${VM}-serial.log}"
BANNER="${BANNER:-M0: WAVE OK}"
TIMEOUT_SECS="${TIMEOUT_SECS:-300}"

echo "=== Step 1: VFIO bind ($GPU) ==="
sudo modprobe vfio-pci
echo "vfio-pci" | sudo tee /sys/bus/pci/devices/$GPU/driver_override > /dev/null
echo "vfio-pci" | sudo tee /sys/bus/pci/devices/$GPU_AUDIO/driver_override > /dev/null
echo "$GPU"       | sudo tee /sys/bus/pci/drivers/vfio-pci/bind 2>/dev/null || true
echo "$GPU_AUDIO" | sudo tee /sys/bus/pci/drivers/snd_hda_intel/unbind 2>/dev/null || true
echo "$GPU_AUDIO" | sudo tee /sys/bus/pci/drivers/vfio-pci/bind 2>/dev/null || true
echo "VFIO bind: $(readlink /sys/bus/pci/devices/$GPU/driver)"

echo ""
echo "=== Step 2: Disable PCIe reset for the GPU (preserve VBIOS POST state) ==="
# THE KEY TRICK: VFIO would FLR the device on assignment, wiping the PSP-SOS/SMU/GC
# POST state the lite:: bring-up needs. Clearing reset_method preserves the last
# cold-POST state into the guest. Flow: cold power-cycle host -> run this -> guest
# sees a freshly-POSTed card.
echo "" | sudo tee /sys/bus/pci/devices/$GPU/reset_method > /dev/null
if [ -f /sys/bus/pci/devices/$GPU_AUDIO/reset_method ]; then
    echo "" | sudo tee /sys/bus/pci/devices/$GPU_AUDIO/reset_method > /dev/null
fi
echo "Reset methods disabled"

echo ""
echo "=== Step 3: Start VM ($VM) ==="
sudo virsh start "$VM"

echo ""
echo "=== Waiting for guest serial banner: '$BANNER' (timeout ${TIMEOUT_SECS}s) ==="
# The nucleus/init/driver-host print to the guest serial, wired to $SERIAL_LOG in
# the domain XML. M0 success = driver-host prints the banner after one wave.
deadline=$(( $(date +%s) + TIMEOUT_SECS ))
while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ -f "$SERIAL_LOG" ] && grep -q "$BANNER" "$SERIAL_LOG" 2>/dev/null; then
        echo "=== $BANNER — M0 dispatch succeeded ==="
        exit 0
    fi
    sleep 3
done
echo "ERROR: banner '$BANNER' not seen within ${TIMEOUT_SECS}s. Last serial output:"
tail -40 "$SERIAL_LOG" 2>/dev/null || echo "(no serial log at $SERIAL_LOG)"
exit 1
