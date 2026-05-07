#!/bin/sh
# Install the atrium-mesa-venus ICD JSON in the system Vulkan-loader
# search path, so frescod / atrium-compositor / vulkaninfo / any
# Vulkan app on this FreeBSD VM auto-discovers it without
# VK_DRIVER_FILES in env.
#
# Run inside the VM. Idempotent.
#
#   vssh "sh /mnt/host/scripts/install-venus-icd.sh"
#
# After this, `vulkaninfo --summary` should list "Virtio-GPU Venus
# (Apple M4 Max)" as GPU0 (or first INTEGRATED_GPU).
set -e

ICD_DIR="/usr/local/share/vulkan/icd.d"
ICD_JSON="$ICD_DIR/atrium_venus.json"
ICD_LIB="${ATRIUM_VENUS_LIB:-/root/mesa/build-atrium/src/virtio/vulkan/libvulkan_virtio.so}"

if [ ! -f "$ICD_LIB" ]; then
    echo "atrium-mesa-venus library not found at: $ICD_LIB" >&2
    echo "Build it inside /root/mesa first, or export ATRIUM_VENUS_LIB=<path>" >&2
    exit 1
fi

if [ ! -d "$ICD_DIR" ]; then
    mkdir -p "$ICD_DIR"
fi

cat > "$ICD_JSON" <<EOF
{
    "file_format_version": "1.0.0",
    "ICD": {
        "library_path": "$ICD_LIB",
        "api_version": "1.4.0"
    }
}
EOF

echo "installed: $ICD_JSON  →  $ICD_LIB"
echo
echo "verify:"
echo "  vulkaninfo --summary | grep -A2 GPU0"
