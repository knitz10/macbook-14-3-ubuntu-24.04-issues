#!/usr/bin/env bash
# Set rEFInd as the first EFI boot option.
# Run as root: sudo bash set-refind-default-boot.sh

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
    echo "Run as root: sudo bash $0"
    exit 1
fi

EFI_MOUNT=/boot/efi
EFI_DISK=/dev/nvme0n1
EFI_PART=1

echo "=== Current EFI boot entries ==="
efibootmgr -v
echo

# Find rEFInd's EFI binary on the EFI partition
REFIND_PATH=$(find "$EFI_MOUNT/EFI" -iname "refind_x64.efi" 2>/dev/null | head -1)

if [[ -z "$REFIND_PATH" ]]; then
    echo "ERROR: refind_x64.efi not found under $EFI_MOUNT/EFI"
    echo "Files present:"
    find "$EFI_MOUNT/EFI" -name "*.efi" 2>/dev/null
    exit 1
fi

echo "Found rEFInd: $REFIND_PATH"

# Convert to EFI-style path (backslashes, relative to EFI partition root)
REL="${REFIND_PATH#$EFI_MOUNT}"
EFI_LOADER="${REL//\//\\}"
echo "EFI loader path: $EFI_LOADER"
echo

# Check if a rEFInd entry already exists
EXISTING_NUM=$(efibootmgr -v 2>/dev/null \
    | grep -i "refind" \
    | grep -oP '(?<=Boot)[0-9A-Fa-f]{4}' \
    | head -1)

if [[ -n "$EXISTING_NUM" ]]; then
    echo "Existing rEFInd entry found: Boot${EXISTING_NUM}"
    BOOT_NUM="$EXISTING_NUM"
else
    echo "No rEFInd entry found — creating one..."
    efibootmgr \
        --create \
        --disk  "$EFI_DISK" \
        --part  "$EFI_PART" \
        --loader "$EFI_LOADER" \
        --label "rEFInd Boot Manager" \
        --unicode ""
    BOOT_NUM=$(efibootmgr -v 2>/dev/null \
        | grep -i "refind" \
        | grep -oP '(?<=Boot)[0-9A-Fa-f]{4}' \
        | head -1)
    echo "Created Boot${BOOT_NUM}"
fi

# Get the current boot order (may be empty on Macs)
CURRENT_ORDER=$(efibootmgr 2>/dev/null | grep '^BootOrder:' | cut -d' ' -f2 || true)
echo "Current boot order: ${CURRENT_ORDER:-(none)}"

# Build new order: rEFInd first, then everything else
if [[ -n "$CURRENT_ORDER" ]]; then
    # Remove rEFInd from existing list, then prepend it
    REST=$(echo "$CURRENT_ORDER" | tr ',' '\n' | grep -iv "^${BOOT_NUM}$" | tr '\n' ',' | sed 's/,$//')
    NEW_ORDER="${BOOT_NUM}${REST:+,$REST}"
else
    NEW_ORDER="$BOOT_NUM"
fi

echo "Setting boot order: $NEW_ORDER"
efibootmgr --bootorder "$NEW_ORDER"

echo
echo "=== Updated EFI boot entries ==="
efibootmgr -v
echo
echo "Done. rEFInd will be the first boot option on next restart."
echo
echo "NOTE: On Apple hardware the firmware may reset the boot order if macOS"
echo "      boots and resets NVRAM. If that happens, re-run this script."
echo "      Alternatively, install rEFInd to the fallback path so the Mac"
echo "      picks it up automatically:"
echo "        sudo mkdir -p $EFI_MOUNT/EFI/BOOT"
echo "        sudo cp '$REFIND_PATH' $EFI_MOUNT/EFI/BOOT/BOOTx64.efi"
