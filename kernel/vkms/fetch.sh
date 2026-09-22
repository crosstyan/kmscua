#!/bin/sh
# Fetch drivers/gpu/drm/vkms from the stable tree matching the running kernel.
# Pass a version when the tree's DRM is newer than its version string says:
# NVIDIA's 5.15.148-tegra carries 5.18-level DRM (iosys_map, shmem wrappers),
# so it takes `./fetch.sh 5.18.19`; the plain 5.15 sources do not compile.
set -e
ver=${1:-$(uname -r | sed 's/-.*//')}
dir=$(dirname "$0")/drivers/gpu/drm/vkms
mkdir -p "$dir"
for f in Makefile vkms_drv.c vkms_drv.h vkms_plane.c vkms_output.c vkms_crtc.c vkms_composer.c vkms_writeback.c; do
  echo "$f"
  curl -sfL "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/plain/drivers/gpu/drm/vkms/$f?h=v$ver" -o "$dir/$f"
done
