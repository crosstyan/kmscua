# Out-of-tree build of drivers/gpu/drm/vkms for the running Jetson kernel, the same
# way uinput.ko is built: the L4T headers package carries the prepared tree (.config,
# Module.symvers, modpost) but no driver sources, so the sources come from the matching
# stable tag via fetch.sh and keep their in-tree paths. CONFIG_DRM_VKMS is off in the
# L4T .config; passing it on the command line overrides it for this build only.
#
#   ./fetch.sh [version]  # pulls drivers/gpu/drm/vkms/*; L4T 36.x needs 5.18.19
#   make                  # builds drivers/gpu/drm/vkms/vkms.ko
#   sudo make install     # copies it under /lib/modules/$(uname -r)/extra and runs depmod
KDIR ?= /lib/modules/$(shell uname -r)/build
CC ?= /usr/bin/gcc

obj-m += drivers/gpu/drm/vkms/

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) CC=$(CC) HOSTCC=$(CC) CONFIG_DRM_VKMS=m modules

install:
	install -D -m 644 drivers/gpu/drm/vkms/vkms.ko /lib/modules/$(shell uname -r)/extra/vkms.ko
	depmod -a

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean
